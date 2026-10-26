//! Daemon <-> GUI/CLI protocol: one JSON object per line over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Action, Config};
use crate::telemetry::Telemetry;

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/opencue-{}", unsafe { libc::getuid() })))
        .join("opencue.sock")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "arg", rename_all = "snake_case")]
pub enum Request {
    Telemetry,
    GetConfig,
    SetConfig(Box<Config>),
    SetProfile(String),
    /// Re-scan USB devices now.
    Rescan,
    /// Run a Nexus action now (the GUI's "Test action" button).
    RunAction(Action),
    /// Ask Discord to authorise OpenCUE (shows a prompt in Discord).
    DiscordConnect,
    /// Forget the Discord login.
    DiscordDisconnect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Telemetry(Box<Telemetry>),
    Config(Box<Config>),
    Error(String),
}

/// A persistent connection to the daemon.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect() -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket_path())?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: stream })
    }

    pub fn request(&mut self, req: &Request) -> std::io::Result<Response> {
        let mut line = serde_json::to_string(req).map_err(std::io::Error::other)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "daemon closed the connection"));
        }
        serde_json::from_str(&reply).map_err(std::io::Error::other)
    }
}
