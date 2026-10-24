//! Now playing: follows MPRIS media players (Spotify, browsers, mpv, VLC, ...) over the session
//! D-Bus, picks the one worth showing and sends it transport commands.
//!
//! Players that are playing win over paused ones, and music players win over everything else,
//! so a paused YouTube tab doesn't hide Spotify. Tapping the track info pins another player.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use opencue_core::ipc::socket_path;
use opencue_core::telemetry::{NowPlaying, unix_ms};
use zbus::blocking::Connection;
use zbus::blocking::fdo::DBusProxy;
use zbus::zvariant::{OwnedValue, Value};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const SEEK_US: i64 = 10_000_000;

const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "m4v", "wmv", "flv", "ts", "mpg", "mpeg", "ogv"];
const AUDIO_EXTS: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "wav", "aac", "wma", "alac", "aiff"];

/// Bus names of apps that are mainly music players.
const MUSIC_APPS: &[&str] = &[
    "spotify",
    "strawberry",
    "elisa",
    "rhythmbox",
    "clementine",
    "lollypop",
    "amarok",
    "cmus",
    "mpd",
    "audacious",
    "quodlibet",
    "tauon",
    "cider",
    "deadbeef",
    "gapless",
    "amberol",
    "g4music",
    "feishin",
    "sonixd",
    "supersonic",
    "youtube-music",
    "youtubemusic",
    "tidal",
    "deezer",
    "plexamp",
    "musikcube",
    "ncspot",
    "spotify_player",
    "psst",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Stopped,
    Paused,
    Playing,
}

#[derive(Debug, Clone)]
struct Player {
    bus: String,
    identity: String,
    status: Status,
    title: String,
    artist: String,
    album: String,
    art_url: String,
    /// The media itself (`xesam:url`), used to make a thumbnail for videos without art.
    url: String,
    length_us: Option<u64>,
    position_us: u64,
    rate: f64,
    music: bool,
}

pub struct Mpris {
    conn: Option<Connection>,
    identities: HashMap<String, String>,
    /// Priority order, best first.
    players: Vec<Player>,
    current: Option<String>,
    /// Player chosen by hand; kept while it plays or nothing else does.
    pinned: Option<String>,
    sampled_ms: u64,
    art: Art,
}

impl Mpris {
    pub fn new() -> Self {
        Self {
            conn: None,
            identities: HashMap::new(),
            players: Vec::new(),
            current: None,
            pinned: None,
            sampled_ms: 0,
            art: Art::default(),
        }
    }

    fn conn(&mut self) -> Result<&Connection> {
        if self.conn.is_none() {
            let conn =
                zbus::blocking::connection::Builder::session()?.method_timeout(Duration::from_secs(1)).build()?;
            self.conn = Some(conn);
        }
        Ok(self.conn.as_ref().unwrap())
    }

    /// Re-read every player. On a D-Bus failure the connection is rebuilt next time.
    pub fn refresh(&mut self) {
        if let Err(e) = self.try_refresh() {
            log::debug!("MPRIS refresh failed: {e:#}");
            self.conn = None;
            self.players.clear();
        }
        self.choose();
    }

    fn try_refresh(&mut self) -> Result<()> {
        let conn = self.conn()?.clone();
        let names: Vec<String> = DBusProxy::new(&conn)?
            .list_names()?
            .into_iter()
            .map(|n| n.to_string())
            .filter(|n| n.starts_with(PREFIX) && !n.ends_with(".playerctld"))
            .collect();
        self.identities.retain(|bus, _| names.contains(bus));
        let mut players = Vec::new();
        for bus in names {
            match read_player(&conn, &bus, &mut self.identities) {
                Ok(p) => players.push(p),
                Err(e) => log::debug!("{bus}: {e:#}"),
            }
        }
        self.sampled_ms = unix_ms();
        // Stable sort keeps D-Bus order for ties; then drop duplicates (a browser and the
        // Plasma browser integration both announce the same tab).
        players.sort_by_key(|p| std::cmp::Reverse((p.status, p.music)));
        let mut seen = Vec::new();
        players.retain(|p| {
            let key = (p.title.clone(), p.artist.clone());
            if p.title.is_empty() || !seen.contains(&key) {
                seen.push(key);
                true
            } else {
                false
            }
        });
        self.players = players;
        Ok(())
    }

    fn choose(&mut self) {
        let find = |bus: &Option<String>| bus.as_ref().and_then(|b| self.players.iter().find(|p| &p.bus == b));
        let best = self.players.first();
        let anything_playing = best.is_some_and(|p| p.status == Status::Playing);
        if find(&self.pinned).is_none_or(|p| p.status != Status::Playing && anything_playing) {
            self.pinned = None;
        }
        self.current = if self.pinned.is_some() {
            self.pinned.clone()
        } else {
            // Stay on the current player unless a better-ranked one shows up, so two paused
            // players don't swap places on every refresh.
            match (find(&self.current), best) {
                (Some(cur), Some(best)) if cur.status >= best.status && cur.music >= best.music => self.current.clone(),
                (_, best) => best.map(|p| p.bus.clone()),
            }
        };
    }

    fn current(&self) -> Option<&Player> {
        self.current.as_ref().and_then(|b| self.players.iter().find(|p| &p.bus == b))
    }

    pub fn now_playing(&mut self) -> Option<NowPlaying> {
        let p = self.current()?.clone();
        Some(NowPlaying {
            player: p.identity,
            title: p.title,
            artist: p.artist,
            album: p.album,
            art: self.art.local_path(&p.art_url, &p.url),
            playing: p.status == Status::Playing,
            length_us: p.length_us,
            position_us: p.position_us,
            rate: p.rate,
            sampled_ms: self.sampled_ms,
            players: self.players.len(),
        })
    }

    pub fn has_player(&self) -> bool {
        self.current().is_some()
    }

    /// Run a `Action::Media` command on the current player.
    pub fn command(&mut self, name: &str) -> Result<()> {
        if name == "switch_player" {
            let n = self.players.len();
            if n > 1 {
                let i = self.current().and_then(|c| self.players.iter().position(|p| p.bus == c.bus)).unwrap_or(0);
                self.pinned = Some(self.players[(i + 1) % n].bus.clone());
                self.current = self.pinned.clone();
            }
            return Ok(());
        }
        let bus = self.current.clone().ok_or_else(|| anyhow!("no media player"))?;
        let conn = self.conn()?.clone();
        let call = |method: &str| conn.call_method(Some(bus.as_str()), PATH, Some(PLAYER_IFACE), method, &());
        match name {
            "play_pause" | "playpause" => call("PlayPause")?,
            "play" => call("Play")?,
            "pause" => call("Pause")?,
            "stop" => call("Stop")?,
            "next" => call("Next")?,
            "previous" | "prev" => call("Previous")?,
            "rewind" | "forward" => {
                let offset = if name == "rewind" { -SEEK_US } else { SEEK_US };
                conn.call_method(Some(bus.as_str()), PATH, Some(PLAYER_IFACE), "Seek", &(offset,))?
            }
            other => bail!("unknown media command {other}"),
        };
        // Show the result straight away rather than on the next poll.
        if let Some(p) = self.players.iter_mut().find(|p| p.bus == bus) {
            match name {
                "play_pause" | "playpause" => {
                    p.status = if p.status == Status::Playing { Status::Paused } else { Status::Playing }
                }
                "rewind" | "forward" => {
                    let pos = p.position_us as i64 + (unix_ms().saturating_sub(self.sampled_ms) as i64 * 1000);
                    let delta = if name == "rewind" { -SEEK_US } else { SEEK_US };
                    p.position_us = (pos + delta).max(0) as u64;
                    self.sampled_ms = unix_ms();
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn read_player(conn: &Connection, bus: &str, identities: &mut HashMap<String, String>) -> Result<Player> {
    let props = get_all(conn, bus, PLAYER_IFACE)?;
    if !identities.contains_key(bus) {
        let identity = get_all(conn, bus, ROOT_IFACE)
            .ok()
            .and_then(|root| root.get("Identity").and_then(|v| as_str(v)).map(String::from))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| bus_app(bus).to_string());
        identities.insert(bus.to_string(), identity);
    }
    let status = match props.get("PlaybackStatus").and_then(|v| as_str(v)) {
        Some("Playing") => Status::Playing,
        Some("Paused") => Status::Paused,
        _ => Status::Stopped,
    };
    let mut meta: HashMap<String, &Value> = HashMap::new();
    if let Some(Value::Dict(d)) = props.get("Metadata").map(|v| unwrap(v)) {
        for (k, v) in d.iter() {
            if let Some(k) = as_str(k) {
                meta.insert(k.to_string(), unwrap(v));
            }
        }
    }
    let text = |key: &str| meta.get(key).map(|v| strings(v).join(", ")).unwrap_or_default();
    let album = text("xesam:album");
    let app = bus_app(bus).to_ascii_lowercase();
    Ok(Player {
        bus: bus.to_string(),
        identity: identities[bus].clone(),
        status,
        title: strip_media_ext(&text("xesam:title")),
        artist: text("xesam:artist"),
        art_url: text("mpris:artUrl"),
        url: text("xesam:url"),
        length_us: meta.get("mpris:length").and_then(|v| as_i64(v)).filter(|&l| l > 0).map(|l| l as u64),
        position_us: props.get("Position").and_then(|v| as_i64(v)).unwrap_or(0).max(0) as u64,
        rate: props.get("Rate").and_then(|v| as_f64(v)).filter(|r| *r > 0.0).unwrap_or(1.0),
        music: MUSIC_APPS.iter().any(|m| app.starts_with(m)) || !album.is_empty(),
        album,
    })
}

fn get_all(conn: &Connection, bus: &str, iface: &str) -> Result<HashMap<String, OwnedValue>> {
    let reply = conn.call_method(Some(bus), PATH, Some("org.freedesktop.DBus.Properties"), "GetAll", &(iface,))?;
    Ok(reply.body().deserialize()?)
}

fn extension(name: &str) -> Option<String> {
    name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map(|(_, ext)| ext.to_ascii_lowercase())
}

/// Players such as mpv use the file name as the title: "Holiday.mp4" -> "Holiday".
fn strip_media_ext(title: &str) -> String {
    match extension(title) {
        Some(ext) if VIDEO_EXTS.contains(&ext.as_str()) || AUDIO_EXTS.contains(&ext.as_str()) => {
            title[..title.len() - ext.len() - 1].to_string()
        }
        _ => title.to_string(),
    }
}

/// "org.mpris.MediaPlayer2.chromium.instance1234" -> "chromium"
fn bus_app(bus: &str) -> &str {
    let rest = bus.strip_prefix(PREFIX).unwrap_or(bus);
    rest.split('.').next().unwrap_or(rest)
}

fn unwrap<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
    match v {
        Value::Value(inner) => unwrap(inner),
        v => v,
    }
}

fn as_str<'a>(v: &'a Value<'a>) -> Option<&'a str> {
    match unwrap(v) {
        Value::Str(s) => Some(s.as_str()),
        Value::ObjectPath(p) => Some(p.as_str()),
        _ => None,
    }
}

fn strings(v: &Value) -> Vec<String> {
    match unwrap(v) {
        Value::Array(a) => a.inner().iter().filter_map(as_str).map(String::from).collect(),
        v => as_str(v).map(String::from).into_iter().collect(),
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    Some(match unwrap(v) {
        Value::I64(x) => *x,
        Value::U64(x) => *x as i64,
        Value::I32(x) => *x as i64,
        Value::U32(x) => *x as i64,
        Value::F64(x) => *x as i64,
        _ => return None,
    })
}

fn as_f64(v: &Value) -> Option<f64> {
    match unwrap(v) {
        Value::F64(x) => Some(*x),
        v => as_i64(v).map(|x| x as f64),
    }
}

/// Cover art as a local file. `file://` URLs are used directly; web URLs are downloaded and local
/// videos without art get a frame grabbed with ffmpeg, both in the background into the runtime
/// directory, where only the current cover is kept.
#[derive(Default)]
struct Art {
    source: String,
    path: Arc<Mutex<Option<String>>>,
}

impl Art {
    fn local_path(&mut self, art_url: &str, media_url: &str) -> Option<String> {
        let source = match media_url.strip_prefix("file://").map(percent_decode) {
            _ if !art_url.is_empty() => art_url.to_string(),
            Some(file) if extension(&file).is_some_and(|e| VIDEO_EXTS.contains(&e.as_str())) => {
                format!("thumbnail:{file}")
            }
            _ => String::new(),
        };
        if source != self.source {
            self.source = source.clone();
            self.path = Arc::new(Mutex::new(None));
            if let Some(file) = source.strip_prefix("file://") {
                *self.path.lock().unwrap() = Some(percent_decode(file));
            } else if source.starts_with("https://") || source.starts_with("http://") {
                self.fetch_in_background(source, |url, out| {
                    run(Command::new("curl")
                        .args(["-sfL", "--max-time", "10", "--max-filesize", "10000000", "-o"])
                        .arg(out)
                        .arg(url))
                });
            } else if source.starts_with("thumbnail:") {
                self.fetch_in_background(source, |source, out| {
                    let video = source.strip_prefix("thumbnail:").unwrap();
                    // A few seconds in skips black intros; short clips fall back to the start.
                    thumbnail(video, out, Some("5")).or_else(|_| thumbnail(video, out, None))
                });
            }
        }
        self.path.lock().unwrap().clone()
    }

    fn fetch_in_background(&self, source: String, make: fn(&str, &Path) -> Result<()>) {
        let slot = self.path.clone();
        thread::spawn(move || match cached(&source, make) {
            Ok(path) => *slot.lock().unwrap() = Some(path),
            Err(e) => log::debug!("cover art {source}: {e:#}"),
        });
    }
}

fn thumbnail(video: &str, out: &Path, seek: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-y"]);
    if let Some(seek) = seek {
        cmd.args(["-ss", seek]);
    }
    cmd.arg("-i").arg(video).args(["-frames:v", "1", "-vf", "scale=160:-2", "-f", "image2", "-c:v", "mjpeg"]).arg(out);
    run(&mut cmd)?;
    if std::fs::metadata(out).map_or(true, |m| m.len() == 0) {
        bail!("no frame");
    }
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status()?;
    if !status.success() {
        bail!("{:?} failed ({status})", cmd.get_program());
    }
    Ok(())
}

/// Make the file for `source` once (into a temp file, then renamed) and keep only it in the cache.
fn cached(source: &str, make: fn(&str, &Path) -> Result<()>) -> Result<String> {
    let dir: PathBuf = socket_path().with_file_name("opencue-art");
    std::fs::create_dir_all(&dir)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    let path = dir.join(format!("{:016x}", h.finish()));
    if !path.exists() {
        let tmp = path.with_extension("part");
        if let Err(e) = make(source, &tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, &path)?;
        for entry in std::fs::read_dir(&dir)?.flatten() {
            if entry.path() != path {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(path.to_string_lossy().into_owned())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(byte) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_urls() {
        assert_eq!(percent_decode("/home/me/My%20Music/cover.jpg"), "/home/me/My Music/cover.jpg");
        assert_eq!(percent_decode("/a%2"), "/a%2");
    }

    #[test]
    fn strips_media_extensions_from_titles() {
        assert_eq!(strip_media_ext("My Holiday Video.mp4"), "My Holiday Video");
        assert_eq!(strip_media_ext("track.FLAC"), "track");
        assert_eq!(strip_media_ext("Mr. Brightside"), "Mr. Brightside");
        assert_eq!(strip_media_ext(".mp4"), ".mp4");
    }

    #[test]
    fn app_from_bus_name() {
        assert_eq!(bus_app("org.mpris.MediaPlayer2.spotify"), "spotify");
        assert_eq!(bus_app("org.mpris.MediaPlayer2.chromium.instance1234"), "chromium");
    }
}
