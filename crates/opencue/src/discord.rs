//! Discord call controls over Discord's local RPC socket (`$XDG_RUNTIME_DIR/discord-ipc-N`).
//!
//! The user's own Discord application is authorised once (Discord shows a prompt); the token is
//! kept in a private file and refreshed before it expires. While connected, OpenCUE follows the
//! mute/deafen state and the current call, and can mute, deafen, toggle the camera and screen
//! share, or leave the call.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use opencue_core::discord::{self, Credentials, Token};
use opencue_core::telemetry::DiscordState;
use serde_json::{Value, json};

pub enum Request {
    /// One of `config::DISCORD_ACTIONS`.
    Action(String),
    Connect,
    Disconnect,
}

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;
const SCOPES: [&str; 5] = ["rpc", "rpc.voice.read", "rpc.voice.write", "rpc.video.write", "rpc.screenshare.write"];
const REDIRECT_URI: &str = "http://localhost";
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
/// Refresh the token when it has less than this left.
const REFRESH_MARGIN: u64 = 24 * 3600;
const RETRY: Duration = Duration::from_secs(5);

/// Runs until `stopping()`; reports state through `publish`.
pub fn run(stopping: impl Fn() -> bool, publish: impl Fn(&DiscordState), requests: Receiver<Request>) {
    let mut authorize = false;
    let mut wait = Duration::ZERO;
    while !stopping() {
        // Between sessions, handle requests while waiting to (re)connect.
        match requests.recv_timeout(wait) {
            Ok(Request::Connect) => authorize = true,
            Ok(Request::Disconnect) => discord::forget_token(),
            Ok(Request::Action(cmd)) => log::warn!("Discord '{cmd}': not connected to Discord"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        wait = Duration::from_secs(1);
        let Some(creds) = discord::load_credentials() else {
            publish(&offline("Set up in OpenCUE: Nexus, Call"));
            continue;
        };
        let token = discord::load_token();
        if token.is_none() && !authorize {
            publish(&offline("Click Connect in OpenCUE"));
            continue;
        }
        match session(&creds, token, &mut authorize, &stopping, &publish, &requests) {
            Ok(()) => {}
            Err(e) => {
                log::debug!("Discord: {e:#}");
                publish(&offline(&e.to_string()));
                wait = RETRY;
            }
        }
    }
}

fn offline(status: &str) -> DiscordState {
    DiscordState { status: status.into(), ..Default::default() }
}

fn session(
    creds: &Credentials,
    token: Option<Token>,
    authorize: &mut bool,
    stopping: &impl Fn() -> bool,
    publish: &impl Fn(&DiscordState),
    requests: &Receiver<Request>,
) -> Result<()> {
    let (mut conn, ready) = Conn::open(&creds.client_id)?;
    let me = ready["user"]["id"].as_str().unwrap_or_default().to_string();

    let mut token = match token {
        Some(t) if !*authorize => t,
        _ => {
            publish(&offline("Approve OpenCUE in Discord"));
            let t = authorize_app(&mut conn, creds)?;
            discord::save_token(&t)?;
            *authorize = false;
            t
        }
    };
    if token.obtained_at + token.expires_in < now() + REFRESH_MARGIN
        && let Ok(t) = refresh(creds, &token)
    {
        discord::save_token(&t)?;
        token = t;
    }
    if let Err(e) = conn.call("AUTHENTICATE", json!({ "access_token": token.access_token })) {
        log::info!("Discord login rejected ({e:#}); refreshing it");
        let Ok(t) = refresh(creds, &token) else {
            discord::forget_token();
            bail!("Discord login expired: click Connect in OpenCUE");
        };
        discord::save_token(&t)?;
        conn.call("AUTHENTICATE", json!({ "access_token": t.access_token }))?;
    }
    for evt in ["VOICE_SETTINGS_UPDATE", "VOICE_CHANNEL_SELECT"] {
        conn.subscribe(evt)?;
    }
    log::info!("Discord connected");
    let mut state = DiscordState { connected: true, ..Default::default() };
    conn.send_command("GET_VOICE_SETTINGS", json!({}))?;
    conn.send_command("GET_SELECTED_VOICE_CHANNEL", json!({}))?;
    publish(&state);

    while !stopping() {
        match requests.try_recv() {
            Ok(Request::Action(cmd)) => {
                if let Err(e) = perform(&mut conn, &state, &cmd) {
                    log::warn!("Discord '{cmd}': {e:#}");
                }
            }
            Ok(Request::Connect) => {}
            Ok(Request::Disconnect) => {
                discord::forget_token();
                log::info!("Discord login forgotten");
                return Ok(());
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }
        let Some(msg) = conn.read()? else { continue };
        let before = state.clone();
        handle(&mut conn, &mut state, &me, &msg)?;
        if state != before {
            publish(&state);
        }
    }
    Ok(())
}

fn perform(conn: &mut Conn, state: &DiscordState, cmd: &str) -> Result<()> {
    let (name, args) = match cmd {
        "mute" => ("SET_VOICE_SETTINGS", json!({ "mute": !state.mute })),
        "deafen" => ("SET_VOICE_SETTINGS", json!({ "deaf": !state.deaf })),
        "camera" => ("TOGGLE_VIDEO", json!({})),
        "screenshare" => ("TOGGLE_SCREENSHARE", json!({})),
        "leave" => ("SELECT_VOICE_CHANNEL", json!({ "channel_id": null })),
        other => bail!("unknown Discord action {other}"),
    };
    log::info!("Discord: {cmd}");
    conn.send_command(name, args).map(|_| ())
}

/// Apply an event or a command reply to `state`.
fn handle(conn: &mut Conn, state: &mut DiscordState, me: &str, msg: &Value) -> Result<()> {
    let data = &msg["data"];
    if msg["evt"] == "ERROR" {
        log::warn!("Discord {}: {}", msg["cmd"].as_str().unwrap_or("?"), data["message"].as_str().unwrap_or("error"));
        return Ok(());
    }
    match (msg["cmd"].as_str(), msg["evt"].as_str()) {
        (Some("DISPATCH"), Some("VOICE_SETTINGS_UPDATE")) | (Some("GET_VOICE_SETTINGS" | "SET_VOICE_SETTINGS"), _) => {
            state.mute = data["mute"].as_bool().unwrap_or(state.mute);
            state.deaf = data["deaf"].as_bool().unwrap_or(state.deaf);
        }
        (Some("DISPATCH"), Some("VOICE_CHANNEL_SELECT")) => {
            if data["channel_id"].is_null() {
                state.in_call = false;
                state.call_name.clear();
            } else {
                conn.send_command("GET_SELECTED_VOICE_CHANNEL", json!({}))?;
            }
        }
        (Some("GET_SELECTED_VOICE_CHANNEL"), _) => {
            state.in_call = data.is_object();
            state.call_name = if state.in_call { call_name(data, me) } else { String::new() };
        }
        _ => {}
    }
    Ok(())
}

/// The channel name, or who a direct call is with.
fn call_name(channel: &Value, me: &str) -> String {
    if let Some(name) = channel["name"].as_str().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    let others: Vec<&str> = channel["voice_states"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["user"]["id"].as_str() != Some(me))
        .filter_map(|v| {
            [&v["nick"], &v["user"]["global_name"], &v["user"]["username"]]
                .into_iter()
                .find_map(|n| n.as_str().filter(|n| !n.is_empty()))
        })
        .collect();
    match others.as_slice() {
        [] => "Call".into(),
        [one] => format!("Call with {one}"),
        [first, rest @ ..] => format!("Call with {first} and {} more", rest.len()),
    }
}

// ---------------------------------------------------------------------------------------------
// OAuth

fn authorize_app(conn: &mut Conn, creds: &Credentials) -> Result<Token> {
    log::info!("asking Discord to authorise OpenCUE");
    let nonce = conn.send_command("AUTHORIZE", json!({ "client_id": creds.client_id, "scopes": SCOPES }))?;
    // The user has to click Authorise in Discord.
    let reply = conn.wait(&nonce, Duration::from_secs(120))?;
    let code = reply["data"]["code"].as_str().ok_or_else(|| anyhow!("Discord sent no authorisation code"))?;
    token_request(creds, &[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", REDIRECT_URI)])
}

fn refresh(creds: &Credentials, token: &Token) -> Result<Token> {
    token_request(creds, &[("grant_type", "refresh_token"), ("refresh_token", &token.refresh_token)])
}

/// POST to Discord's token endpoint. The body (with the client secret) goes to curl on stdin,
/// never on its command line, which other users can read.
fn token_request(creds: &Credentials, fields: &[(&str, &str)]) -> Result<Token> {
    let mut all = vec![("client_id", creds.client_id.as_str()), ("client_secret", creds.client_secret.as_str())];
    all.extend_from_slice(fields);
    let body = form_encode(&all);
    let mut child = Command::new("curl")
        .args(["-sS", "--max-time", "15", "--proto", "=https", "-X", "POST"])
        .args(["-H", "Content-Type: application/x-www-form-urlencoded", "-H", "User-Agent: OpenCUE"])
        .args(["--data-binary", "@-", TOKEN_URL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run curl")?;
    child.stdin.take().unwrap().write_all(body.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("cannot reach Discord: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let reply: Value = serde_json::from_slice(&out.stdout).context("unexpected reply from Discord")?;
    if let Some(err) = reply["error"].as_str() {
        bail!("Discord refused the login: {}", reply["error_description"].as_str().unwrap_or(err));
    }
    Ok(Token {
        access_token: reply["access_token"].as_str().ok_or_else(|| anyhow!("no access token"))?.into(),
        refresh_token: reply["refresh_token"].as_str().unwrap_or_default().into(),
        expires_in: reply["expires_in"].as_u64().unwrap_or(0),
        obtained_at: now(),
    })
}

fn form_encode(fields: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    fields.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------------------------------------
// IPC framing: <op u32 LE><len u32 LE><JSON>

struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
    nonce: u64,
}

fn socket_paths() -> Vec<PathBuf> {
    let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else { return Vec::new() };
    // Native, Flatpak and Snap Discord.
    let dirs = [run.clone(), run.join("app/com.discordapp.Discord"), run.join("snap.discord")];
    dirs.iter().flat_map(|d| (0..10).map(move |i| d.join(format!("discord-ipc-{i}")))).collect()
}

impl Conn {
    fn open(client_id: &str) -> Result<(Self, Value)> {
        let stream = socket_paths()
            .into_iter()
            .find_map(|p| UnixStream::connect(p).ok())
            .ok_or_else(|| anyhow!("Discord isn't running"))?;
        stream.set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut conn = Self { stream, buf: Vec::new(), nonce: 0 };
        conn.send(OP_HANDSHAKE, &json!({ "v": 1, "client_id": client_id }))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(msg) = conn.read()?
                && msg["evt"] == "READY"
            {
                return Ok((conn, msg["data"].clone()));
            }
        }
        bail!("Discord did not answer")
    }

    fn send(&mut self, op: u32, payload: &Value) -> Result<()> {
        let body = serde_json::to_vec(payload)?;
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        self.stream.write_all(&frame).context("lost the connection to Discord")
    }

    fn send_command(&mut self, cmd: &str, args: Value) -> Result<String> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        self.send(OP_FRAME, &json!({ "cmd": cmd, "args": args, "nonce": nonce }))?;
        Ok(nonce)
    }

    fn subscribe(&mut self, evt: &str) -> Result<()> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        self.send(OP_FRAME, &json!({ "cmd": "SUBSCRIBE", "evt": evt, "args": {}, "nonce": nonce }))?;
        self.wait(&nonce, Duration::from_secs(5)).map(|_| ())
    }

    /// Send a command and wait for its reply.
    fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
        let nonce = self.send_command(cmd, args)?;
        self.wait(&nonce, Duration::from_secs(10))
    }

    fn wait(&mut self, nonce: &str, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let Some(msg) = self.read()? else { continue };
            if msg["nonce"] == nonce {
                if msg["evt"] == "ERROR" {
                    bail!("{}", msg["data"]["message"].as_str().unwrap_or("Discord returned an error"));
                }
                return Ok(msg);
            }
        }
        bail!("Discord did not reply in time")
    }

    /// The next message, or `None` if nothing arrived within the read timeout.
    fn read(&mut self) -> Result<Option<Value>> {
        loop {
            if let Some((op, body)) = take_frame(&mut self.buf) {
                match op {
                    OP_FRAME => return Ok(Some(serde_json::from_slice(&body)?)),
                    OP_PING => self.send(OP_PONG, &serde_json::from_slice(&body)?)?,
                    OP_CLOSE => bail!("Discord closed the connection: {}", String::from_utf8_lossy(&body)),
                    _ => {}
                }
                continue;
            }
            let mut chunk = [0u8; 16 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => bail!("Discord closed"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Ok(None),
                Err(e) => return Err(e).context("lost the connection to Discord"),
            }
        }
    }
}

/// Split one complete frame off the front of `buf`.
fn take_frame(buf: &mut Vec<u8>) -> Option<(u32, Vec<u8>)> {
    if buf.len() < 8 {
        return None;
    }
    let op = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    if buf.len() < 8 + len {
        return None;
    }
    let body = buf[8..8 + len].to_vec();
    buf.drain(..8 + len);
    Some((op, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_reads() {
        let body = br#"{"evt":"READY"}"#;
        let mut wire = 1u32.to_le_bytes().to_vec();
        wire.extend_from_slice(&(body.len() as u32).to_le_bytes());
        wire.extend_from_slice(body);
        let mut buf = wire[..5].to_vec();
        assert_eq!(take_frame(&mut buf), None);
        buf.extend_from_slice(&wire[5..]);
        buf.extend_from_slice(&wire[..3]);
        assert_eq!(take_frame(&mut buf), Some((1, body.to_vec())));
        assert_eq!(buf, wire[..3]);
    }

    #[test]
    fn form_encoding() {
        assert_eq!(form_encode(&[("a", "x y&z"), ("b", "ok-1.2_~")]), "a=x%20y%26z&b=ok-1.2_~");
    }

    #[test]
    fn call_names() {
        let me = "1";
        assert_eq!(call_name(&json!({ "name": "General" }), me), "General");
        let dm = json!({ "name": null, "voice_states": [
            { "user": { "id": "1", "username": "me" } },
            { "nick": null, "user": { "id": "2", "username": "alex", "global_name": "Alex" } },
        ]});
        assert_eq!(call_name(&dm, me), "Call with Alex");
        let group = json!({ "voice_states": [
            { "nick": "Sam", "user": { "id": "2" } },
            { "user": { "id": "3", "username": "kim" } },
            { "user": { "id": "4", "username": "lee" } },
        ]});
        assert_eq!(call_name(&group, me), "Call with Sam and 2 more");
    }

    #[test]
    fn state_follows_events() {
        // A dummy connection is never written to by these events.
        let (a, _b) = UnixStream::pair().unwrap();
        let mut conn = Conn { stream: a, buf: Vec::new(), nonce: 0 };
        let mut s = DiscordState { connected: true, ..Default::default() };
        let ev = json!({ "cmd": "DISPATCH", "evt": "VOICE_SETTINGS_UPDATE", "data": { "mute": true, "deaf": false } });
        handle(&mut conn, &mut s, "1", &ev).unwrap();
        assert!(s.mute && !s.deaf);
        let ev = json!({ "cmd": "GET_SELECTED_VOICE_CHANNEL", "data": { "name": "General" } });
        handle(&mut conn, &mut s, "1", &ev).unwrap();
        assert!(s.in_call);
        assert_eq!(s.call_name, "General");
        let ev = json!({ "cmd": "DISPATCH", "evt": "VOICE_CHANNEL_SELECT", "data": { "channel_id": null } });
        handle(&mut conn, &mut s, "1", &ev).unwrap();
        assert!(!s.in_call);
    }
}
