//! Game stats from MangoHud's per-frame CSV log: FPS, frame-time lows and stutters.
//!
//! MangoHud already measures every frame inside the game. With `autostart_log=1`,
//! `log_interval=0` and `output_folder` pointing at [`log_dir`], it appends one row per frame
//! to a CSV while the game runs; [`LogTracker`] follows the newest file and keeps a rolling
//! window of frame times.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Lows and stutters are measured over this much recent play.
pub const WINDOW: Duration = Duration::from_secs(30);
/// A log not written to for this long belongs to a game that has closed.
const IDLE: Duration = Duration::from_secs(3);
/// Logs of closed games are deleted after this long.
const KEEP: Duration = Duration::from_secs(120);
/// When joining a running game, read only the end of its log (about 30 s at 144 FPS).
const TAIL_BYTES: u64 = 640 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameStats {
    /// Executable name from the log file, e.g. "eldenring.exe".
    pub game: String,
    pub fps: f32,
    pub frametime_ms: f32,
    /// FPS at the 99th / 99.9th percentile frame time.
    pub low1: f32,
    pub low01: f32,
    pub p99_ms: f32,
    /// Frames in the window that took more than twice the median (and at least 8 ms longer).
    pub stutters: u32,
}

/// Where OpenCUE asks MangoHud to write its logs.
pub fn log_dir() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".cache"));
    cache.join("opencue").join("mangohud")
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

// ---------------------------------------------------------------------------------------------
// MangoHud configuration

const BLOCK_START: &str = "# >>> OpenCUE game stats (managed by OpenCUE; delete this block to turn it off)";
const BLOCK_END: &str = "# <<< OpenCUE game stats";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MangoHudStatus {
    NotInstalled,
    NotSetUp,
    Ready,
}

pub fn mangohud_config() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"));
    config.join("MangoHud").join("MangoHud.conf")
}

fn mangohud_installed() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("mangohud").is_file()))
}

pub fn mangohud_status() -> MangoHudStatus {
    if !mangohud_installed() {
        return MangoHudStatus::NotInstalled;
    }
    let text = fs::read_to_string(mangohud_config()).unwrap_or_default();
    if text.contains(BLOCK_START) { MangoHudStatus::Ready } else { MangoHudStatus::NotSetUp }
}

/// Add (or refresh) OpenCUE's block at the end of the global MangoHud config.
pub fn setup_mangohud() -> io::Result<PathBuf> {
    let path = mangohud_config();
    let text = fs::read_to_string(&path).unwrap_or_default();
    let mut out = without_block(&text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&block(&log_dir()));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("conf.opencue-tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

fn block(dir: &Path) -> String {
    format!(
        "{BLOCK_START}\n# Log every frame to a file OpenCUE reads for the Nexus, and deletes after the game.\nautostart_log=1\nlog_interval=0\noutput_folder={}\n{BLOCK_END}\n",
        dir.display()
    )
}

fn without_block(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        match line {
            l if l == BLOCK_START => inside = true,
            l if l == BLOCK_END => inside = false,
            l if !inside => {
                out.push_str(l);
                out.push('\n');
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Following the log

/// One frame: when it ended (ns since the game started) and how long it took.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Frame {
    at_ns: u64,
    ms: f32,
}

#[derive(Default)]
pub struct LogTracker {
    path: Option<PathBuf>,
    file: Option<File>,
    columns: Option<(usize, usize)>,
    partial: String,
    /// Joined mid-file: the first line read is the tail of a row.
    skip_line: bool,
    frames: VecDeque<Frame>,
}

impl LogTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read whatever the game has logged since the last call. `None` when no game is running.
    pub fn poll(&mut self) -> Option<GameStats> {
        let dir = log_dir();
        let Some(newest) = newest_log(&dir).filter(|(_, age)| *age < IDLE).map(|(p, _)| p) else {
            *self = Self::default();
            return None;
        };
        if self.path.as_ref() != Some(&newest) {
            *self = Self::default();
            if let Err(e) = self.open(&newest) {
                log::debug!("cannot read {}: {e}", newest.display());
                return None;
            }
        }
        if let Err(e) = self.read_new() {
            log::debug!("cannot read {}: {e}", newest.display());
            *self = Self::default();
            return None;
        }
        let game = game_name(&newest);
        stats(&self.frames).map(|s| GameStats { game, ..s })
    }

    fn open(&mut self, path: &Path) -> io::Result<()> {
        let mut file = File::open(path)?;
        let mut head = vec![0; 4096];
        let n = file.read(&mut head)?;
        let head = String::from_utf8_lossy(&head[..n]).into_owned();
        // Two lines of system info, then the column names.
        let header = head.lines().find(|l| l.starts_with("fps,")).ok_or(io::ErrorKind::InvalidData)?;
        self.columns = columns(header);
        if self.columns.is_none() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let len = file.metadata()?.len();
        let start = len.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        self.skip_line = start > 0;
        self.path = Some(path.to_path_buf());
        self.file = Some(file);
        Ok(())
    }

    fn read_new(&mut self) -> io::Result<()> {
        let (Some(file), Some((ft, at))) = (self.file.as_mut(), self.columns) else { return Ok(()) };
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;
        self.partial.push_str(&buf);
        if self.skip_line {
            let Some(end) = self.partial.find('\n') else { return Ok(()) };
            self.partial.drain(..=end);
            self.skip_line = false;
        }
        let complete = self.partial.rfind('\n').map_or(0, |i| i + 1);
        for line in self.partial[..complete].lines() {
            if let Some(frame) = parse_row(line, ft, at) {
                self.frames.push_back(frame);
            }
        }
        self.partial.drain(..complete);
        if let Some(last) = self.frames.back().map(|f| f.at_ns) {
            let oldest = last.saturating_sub(WINDOW.as_nanos() as u64);
            while self.frames.front().is_some_and(|f| f.at_ns < oldest) {
                self.frames.pop_front();
            }
        }
        Ok(())
    }
}

/// Indexes of the `frametime` and `elapsed` columns.
fn columns(header: &str) -> Option<(usize, usize)> {
    let names: Vec<&str> = header.trim().split(',').collect();
    Some((names.iter().position(|n| *n == "frametime")?, names.iter().position(|n| *n == "elapsed")?))
}

fn parse_row(line: &str, ft: usize, at: usize) -> Option<Frame> {
    let cells: Vec<&str> = line.trim().split(',').collect();
    let ms: f32 = cells.get(ft)?.parse().ok()?;
    let at_ns: u64 = cells.get(at)?.parse().ok()?;
    (ms > 0.0 && ms.is_finite()).then_some(Frame { at_ns, ms })
}

fn stats(frames: &VecDeque<Frame>) -> Option<GameStats> {
    let last = frames.back()?;
    // Current FPS and frame time: the last second of frames.
    let second_ago = last.at_ns.saturating_sub(1_000_000_000);
    let recent: Vec<f32> = frames.iter().rev().take_while(|f| f.at_ns >= second_ago).map(|f| f.ms).collect();
    let recent_ms = recent.iter().sum::<f32>() / recent.len() as f32;

    let mut sorted: Vec<f32> = frames.iter().map(|f| f.ms).collect();
    sorted.sort_by(f32::total_cmp);
    let pct = |p: f32| sorted[(((sorted.len() - 1) as f32) * p).round() as usize];
    let (median, p99, p999) = (pct(0.5), pct(0.99), pct(0.999));
    let stutters = frames.iter().filter(|f| f.ms > 2.0 * median && f.ms - median >= 8.0).count() as u32;
    Some(GameStats {
        game: String::new(),
        fps: 1000.0 / recent_ms,
        frametime_ms: recent_ms,
        low1: 1000.0 / p99,
        low01: 1000.0 / p999,
        p99_ms: p99,
        stutters,
    })
}

/// "eldenring.exe_2026-10-26_17-51-06.csv" -> "eldenring.exe"
fn game_name(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let mut parts = stem.rsplitn(3, '_');
    let (_time, _date, name) = (parts.next(), parts.next(), parts.next());
    name.unwrap_or(stem).to_string()
}

/// The most recently written per-frame log (not a summary) and how long ago it was written.
fn newest_log(dir: &Path) -> Option<(PathBuf, Duration)> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".csv") && !n.ends_with("_summary.csv")))
        .filter_map(|e| {
            let age = SystemTime::now().duration_since(e.metadata().ok()?.modified().ok()?).unwrap_or_default();
            Some((e.path(), age))
        })
        .min_by_key(|(_, age)| *age)
}

/// Delete logs of games that closed a while ago; they grow by ~15 KB per second of play.
pub fn clean_old_logs() {
    let Ok(entries) = fs::read_dir(log_dir()) else { return };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > KEEP);
        if old && e.file_name().to_str().is_some_and(|n| n.ends_with(".csv")) {
            let _ = fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(ms: &[f32]) -> VecDeque<Frame> {
        let mut at = 0;
        ms.iter()
            .map(|&ms| {
                at += (ms * 1_000_000.0) as u64;
                Frame { at_ns: at, ms }
            })
            .collect()
    }

    #[test]
    fn steady_frames() {
        let s = stats(&frames(&[16.667; 600])).unwrap();
        assert!((s.fps - 60.0).abs() < 0.1, "{s:?}");
        assert!((s.low1 - 60.0).abs() < 0.1);
        assert_eq!(s.stutters, 0);
    }

    #[test]
    fn spikes_lower_the_lows_and_count_as_stutters() {
        let mut ms = vec![10.0; 990];
        ms.extend([50.0; 10]);
        let s = stats(&frames(&ms)).unwrap();
        assert_eq!(s.stutters, 10);
        // Exactly 1% of frames are slow, so the 99th percentile is the last fast one.
        assert_eq!(s.p99_ms, 10.0);
        assert!((s.low01 - 20.0).abs() < 0.1, "{s:?}");
    }

    #[test]
    fn small_wobbles_at_high_fps_are_not_stutters() {
        let mut ms = vec![3.0; 500];
        ms.extend([7.0; 5]);
        assert_eq!(stats(&frames(&ms)).unwrap().stutters, 0);
    }

    #[test]
    fn reads_mangohud_rows() {
        let header = "fps,frametime,cpu_load,cpu_power,gpu_load,cpu_temp,gpu_temp,gpu_core_clock,gpu_mem_clock,gpu_vram_used,gpu_power,ram_used,swap_used,process_rss,cpu_mhz,elapsed";
        let (ft, at) = columns(header).unwrap();
        let row = "59.9792,16.6725,4.41527,0,9,41,51,895,772,1.72861,37,5.28751,0.000320435,0,3597,100598006";
        assert_eq!(parse_row(row, ft, at), Some(Frame { at_ns: 100598006, ms: 16.6725 }));
        assert_eq!(parse_row("garbage", ft, at), None);
    }

    #[test]
    fn game_names_from_log_files() {
        assert_eq!(game_name(Path::new("/x/vkcube_2026-10-26_17-51-06.csv")), "vkcube");
        assert_eq!(game_name(Path::new("/x/Cyberpunk2077.exe_2026-10-26_17-51-06.csv")), "Cyberpunk2077.exe");
        assert_eq!(game_name(Path::new("/x/my_game_2026-10-26_17-51-06.csv")), "my_game");
    }

    #[test]
    fn config_block_is_replaced_not_duplicated() {
        let user = "fps_limit=144\n";
        let once = format!("{}{}", without_block(user), block(Path::new("/c/opencue/mangohud")));
        let twice = format!("{}{}", without_block(&once), block(Path::new("/c/opencue/mangohud")));
        assert_eq!(once, twice);
        assert!(once.starts_with("fps_limit=144\n"));
        assert_eq!(without_block(&once), user);
    }
}
