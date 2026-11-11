//! Update checks against the GitHub releases, and self-update for user installs.
//!
//! The check is one request to GitHub's public API; nothing about the user is sent. Installs in
//! `~/.local/bin` (the release download or a source build, both via install.sh) can update
//! themselves: download the release, verify its checksum, and run its install.sh. Installs from
//! a package manager are told to update with it instead.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const REPO: &str = "lfreixial/opencue";
const ASSET: &str = "opencue-x86_64-linux.tar.gz";

/// How this copy was installed, which decides how it updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// `~/.local/bin`, by install.sh: can update itself.
    User,
    /// `/usr`, by a package manager (e.g. the AUR): updates come from it.
    Package,
    /// Anywhere else, e.g. running from a source tree's `target/`.
    Other,
}

pub fn install_kind() -> Install {
    let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else { return Install::Other };
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    classify(&exe, &home)
}

fn classify(exe: &Path, home: &Path) -> Install {
    if !home.as_os_str().is_empty() && exe.parent() == Some(&home.join(".local/bin")) {
        Install::User
    } else if exe.starts_with("/usr") {
        Install::Package
    } else {
        Install::Other
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Without the leading `v`, e.g. "0.1.2".
    pub version: String,
    pub url: String,
}

/// The latest published release.
pub fn latest() -> Result<Release> {
    let body = curl(&[
        "-H",
        "Accept: application/vnd.github+json",
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
    ])?;
    parse_release(&body)
}

fn parse_release(body: &[u8]) -> Result<Release> {
    let v: serde_json::Value = serde_json::from_slice(body).context("unexpected reply from GitHub")?;
    let tag = v["tag_name"].as_str().context("no release found")?;
    let url = v["html_url"].as_str().unwrap_or_default();
    Ok(Release { version: tag.trim_start_matches('v').to_string(), url: url.to_string() })
}

/// Whether `candidate` is a newer version than `current` (both `x.y.z`).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

/// Download `version`, check it against its published checksum, and install it with its own
/// install.sh, which also restarts the background service. The app is restarted by the caller.
pub fn install(version: &str) -> Result<()> {
    if std::env::consts::ARCH != "x86_64" {
        bail!("prebuilt updates are only available for x86_64; update from source instead");
    }
    let dir = update_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let base = format!("https://github.com/{REPO}/releases/download/v{version}/{ASSET}");
    let tarball = dir.join(ASSET);
    let sums = dir.join(format!("{ASSET}.sha256"));
    curl(&["-o", &tarball.to_string_lossy(), &base])?;
    curl(&["-o", &sums.to_string_lossy(), &format!("{base}.sha256")])?;

    run(Command::new("sha256sum").args(["--check", "--status"]).arg(&sums).current_dir(&dir))
        .context("the download doesn't match its checksum")?;
    run(Command::new("tar").arg("xzf").arg(&tarball).current_dir(&dir)).context("cannot unpack the download")?;
    run(&mut Command::new(dir.join("opencue-linux/install.sh"))).context("install.sh failed")?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Start the newly installed app once this one has exited (it owns the single-window socket).
pub fn relaunch() {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let _ = Command::new("sh")
        .args(["-c", "sleep 1; exec \"$0\" --page settings"])
        .arg(home.join(".local/bin/opencue-gui"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn update_dir() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".cache"));
    cache.join("opencue/update")
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "120", "-A", "opencue-updater"])
        .args(args)
        .output()
        .context("curl is needed to check for updates")?;
    if !out.status.success() {
        bail!("cannot reach GitHub: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("cannot run {:?}", cmd.get_program()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.lines().last().unwrap_or("failed").trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1"));
        assert!(!is_newer("0.2", "0.1.1"));
    }

    #[test]
    fn reads_the_release_reply() {
        let body = br#"{"tag_name":"v0.1.2","html_url":"https://github.com/lfreixial/opencue/releases/tag/v0.1.2"}"#;
        let r = parse_release(body).unwrap();
        assert_eq!(r.version, "0.1.2");
        assert!(r.url.ends_with("/v0.1.2"));
        assert!(parse_release(br#"{"message":"Not Found"}"#).is_err());
    }

    #[test]
    fn install_kind_follows_the_binary_location() {
        let home = Path::new("/home/me");
        assert_eq!(classify(Path::new("/home/me/.local/bin/opencue-gui"), home), Install::User);
        assert_eq!(classify(Path::new("/usr/bin/opencue-gui"), home), Install::Package);
        assert_eq!(classify(Path::new("/home/me/src/opencue/target/release/opencue-gui"), home), Install::Other);
    }
}
