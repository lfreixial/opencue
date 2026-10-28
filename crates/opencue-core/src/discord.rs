//! Discord application credentials and the saved login, shared by the daemon and the GUI.
//!
//! Discord only lets an application control voice through its local RPC socket once the user has
//! authorised it, so each user registers their own application (free, about a minute) and
//! OpenCUE keeps its ID and secret, and the resulting token, in private files.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds the access token is valid for, from `obtained_at`.
    pub expires_in: u64,
    /// Unix time.
    pub obtained_at: u64,
}

pub fn credentials_path() -> PathBuf {
    config_dir().join("discord.json")
}

pub fn token_path() -> PathBuf {
    config_dir().join("discord-token.json")
}

pub fn load_credentials() -> Option<Credentials> {
    let c: Credentials = serde_json::from_str(&fs::read_to_string(credentials_path()).ok()?).ok()?;
    (!c.client_id.trim().is_empty() && !c.client_secret.trim().is_empty()).then_some(c)
}

pub fn save_credentials(c: &Credentials) -> io::Result<()> {
    write_private(&credentials_path(), &serde_json::to_vec_pretty(c)?)
}

pub fn load_token() -> Option<Token> {
    serde_json::from_str(&fs::read_to_string(token_path()).ok()?).ok().filter(|t: &Token| !t.access_token.is_empty())
}

pub fn save_token(t: &Token) -> io::Result<()> {
    write_private(&token_path(), &serde_json::to_vec(t)?)
}

pub fn forget_token() {
    let _ = fs::remove_file(token_path());
}

/// Write a file only the user can read (0600), replacing it atomically.
fn write_private(path: &PathBuf, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}
