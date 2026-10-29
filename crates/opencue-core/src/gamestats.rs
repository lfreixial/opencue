//! Game stats from MangoHud's per-frame CSV log: FPS, frame-time lows and stutters.
//!
//! MangoHud already measures every frame inside the game. With `autostart_log=1`,
//! `log_interval=0` and `output_folder` pointing at [`log_dir`], it appends one row per frame
//! to a CSV while the game runs; [`LogTracker`] follows the newest file and keeps a rolling
//! window of frame times.
//!
//! Setup also sets `MANGOHUD=1` for the whole login session (see [`env_file`]) and adds
//! makes the overlay fully transparent, so every Vulkan game logs without a per-game launch
//! option and without anything drawn over it. (`no_display` would hide it too, but MangoHud then
//! stops logging.)

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
}

pub fn mangohud_config() -> PathBuf {
    config_home().join("MangoHud").join("MangoHud.conf")
}

fn mangohud_installed() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("mangohud").is_file()))
}

pub fn mangohud_status() -> MangoHudStatus {
    if !mangohud_installed() {
        return MangoHudStatus::NotInstalled;
    }
    // An older block (or a missing env file) counts as not set up, so the app offers to update it.
    let text = fs::read_to_string(mangohud_config()).unwrap_or_default();
    let env = fs::read_to_string(env_file()).unwrap_or_default();
    if current_block(&text) && env == ENV { MangoHudStatus::Ready } else { MangoHudStatus::NotSetUp }
}

/// Sets `MANGOHUD=1` for every app started in the login session, from the next login.
pub fn env_file() -> PathBuf {
    config_home().join("environment.d").join("opencue-mangohud.conf")
}

const ENV: &str =
    "# Written by OpenCUE: load MangoHud (hidden) into every Vulkan game, for the Nexus game stats.\nMANGOHUD=1\n";

/// Add (or refresh) OpenCUE's block at the end of the global MangoHud config, and turn MangoHud on
/// for the session.
pub fn setup_mangohud() -> io::Result<PathBuf> {
    let path = mangohud_config();
    let text = fs::read_to_string(&path).unwrap_or_default();
    let mut out = without_block(&text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    // Keeps the overlay as it was (hidden for a new setup).
    out.push_str(&block(&log_dir(), overlay_visible()));
    // MangoHud does not create its output folder; without it, it silently logs nothing.
    fs::create_dir_all(log_dir())?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("conf.opencue-tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, &path)?;
    let env = env_file();
    if let Some(dir) = env.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(env, ENV)?;
    Ok(path)
}

const HIDDEN: &str =
    "# Hidden by making it transparent: no_display would also stop the logging.\nalpha=0\nbackground_alpha=0\n";

fn block(dir: &Path, visible: bool) -> String {
    format!(
        "{BLOCK_START}\n# Log every frame to a file OpenCUE reads for the Nexus, and deletes after the game.\n{}# MangoHud's own toggle sets no_display, which stops the logging; `opencue overlay` toggles instead.\ntoggle_hud=\nautostart_log=1\nlog_interval=0\nlog_duration=0\noutput_folder={}\n{BLOCK_END}\n",
        if visible { "" } else { HIDDEN },
        dir.display()
    )
}

/// Whether OpenCUE's block leaves MangoHud's overlay visible (it's hidden unless asked for).
pub fn overlay_visible() -> bool {
    block_shows_overlay(&fs::read_to_string(mangohud_config()).unwrap_or_default())
}

/// Reads the settings, not the exact text, so blocks from older versions count too (those hid it
/// with `no_display`).
fn block_shows_overlay(text: &str) -> bool {
    let mut inside = false;
    let mut found = false;
    for line in text.lines() {
        match line.trim() {
            l if l == BLOCK_START => (inside, found) = (true, true),
            l if l == BLOCK_END => inside = false,
            "alpha=0" | "no_display" | "no_display=1" if inside => return false,
            _ => {}
        }
    }
    found
}

/// Show or hide MangoHud's overlay. Running games reload the config within a moment: MangoHud
/// starts a new log, which [`LogTracker`] carries on from.
pub fn set_overlay_visible(visible: bool) -> io::Result<()> {
    let path = mangohud_config();
    let text = fs::read_to_string(&path)?;
    if !text.contains(BLOCK_START) {
        return Err(io::Error::other("MangoHud is not set up for OpenCUE yet"));
    }
    let mut out = without_block(&text);
    out.push_str(&block(&log_dir(), visible));
    let tmp = path.with_extension("conf.opencue-tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, &path)
}

fn current_block(text: &str) -> bool {
    let dir = log_dir();
    text.contains(&block(&dir, false)) || text.contains(&block(&dir, true))
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
    /// Added to the new log's times when carrying on from the game's previous log.
    offset_ns: u64,
    /// Size of each active log at the last poll, to tell which one is the game.
    sizes: HashMap<PathBuf, u64>,
    last_poll: Option<Instant>,
}

impl LogTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read whatever the game has logged since the last call. `None` when no game is running.
    pub fn poll(&mut self) -> Option<GameStats> {
        let logs = active_logs(&log_dir());
        let secs = self.last_poll.map_or(0.0, |t| t.elapsed().as_secs_f64());
        self.last_poll = Some(Instant::now());
        let games = if logs.is_empty() { HashSet::new() } else { game_processes() };
        let chosen = choose_log(&logs, &self.sizes, secs, |name| is_game(&games, name));
        self.sizes = logs.into_iter().collect();
        let Some(chosen) = chosen else {
            self.reset();
            return None;
        };
        if self.path.as_ref() != Some(&chosen) {
            // MangoHud starts a new log when its config is reloaded (e.g. `opencue overlay`): carry
            // on from the old one instead of starting the lows and stutters from scratch.
            let same_game = self.path.as_deref().map(game_name) == Some(game_name(&chosen));
            let frames = std::mem::take(&mut self.frames);
            self.reset();
            if let Err(e) = self.open(&chosen) {
                log::debug!("cannot read {}: {e}", chosen.display());
                return None;
            }
            if same_game {
                self.offset_ns = frames.back().map_or(0, |f| f.at_ns);
                self.frames = frames;
            }
        }
        if let Err(e) = self.read_new() {
            log::debug!("cannot read {}: {e}", chosen.display());
            self.reset();
            return None;
        }
        let game = game_name(&chosen);
        stats(&self.frames).map(|s| GameStats { game, ..s })
    }

    /// Forget the log being followed (but not the sizes).
    fn reset(&mut self) {
        *self = Self { sizes: std::mem::take(&mut self.sizes), last_poll: self.last_poll, ..Self::default() };
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
                self.frames.push_back(Frame { at_ns: frame.at_ns + self.offset_ns, ..frame });
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

/// Per-frame logs (not summaries) written to in the last few seconds, with their sizes.
fn active_logs(dir: &Path) -> Vec<(PathBuf, u64)> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".csv") && !n.ends_with("_summary.csv")))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let age = SystemTime::now().duration_since(meta.modified().ok()?).unwrap_or_default();
            (age < IDLE).then(|| (e.path(), meta.len()))
        })
        .collect()
}

/// Logs written slower than this (about 20 FPS of 88-byte rows) only count if a launcher started them.
const MIN_BYTES_PER_SEC: f64 = 20.0 * 88.0;

/// Which log is the game's. Every Vulkan app loads MangoHud, so a terminal or browser logs too
/// whenever it redraws. Logs of processes a game launcher started come first; otherwise only one
/// written at a game-like frame rate counts. Among those, the one that grew most since `secs` ago.
fn choose_log(
    logs: &[(PathBuf, u64)],
    before: &HashMap<PathBuf, u64>,
    secs: f64,
    is_game: impl Fn(&str) -> bool,
) -> Option<PathBuf> {
    let growth = |(path, size): &&(PathBuf, u64)| size.saturating_sub(before.get(path).copied().unwrap_or(0));
    let busiest = |logs: Vec<&(PathBuf, u64)>| logs.into_iter().max_by_key(growth).map(|(path, _)| path.clone());
    let games: Vec<_> = logs.iter().filter(|(path, _)| is_game(&game_name(path))).collect();
    if !games.is_empty() {
        return busiest(games);
    }
    let fast = |log: &&(PathBuf, u64)| {
        before.contains_key(&log.0) && secs > 0.0 && growth(log) as f64 >= MIN_BYTES_PER_SEC * secs
    };
    busiest(logs.iter().filter(fast).collect())
}

/// Environment variables that game launchers set: Steam, Proton and Wine (so also Heroic and
/// Bottles), and Lutris.
const LAUNCHER_VARS: [&[u8]; 6] = [
    b"SteamAppId=",
    b"SteamGameId=",
    b"STEAM_COMPAT_APP_ID=",
    b"STEAM_COMPAT_DATA_PATH=",
    b"WINEPREFIX=",
    b"LUTRIS_GAME_UUID=",
];

/// Names of running processes a game launcher started: their `comm` (cut to 15 characters by the
/// kernel) and the file name they were started as (Wine uses Windows paths).
fn game_processes() -> HashSet<String> {
    let mut names = HashSet::new();
    let Ok(procs) = fs::read_dir("/proc") else { return names };
    for p in procs.flatten() {
        let dir = p.path();
        let Ok(env) = fs::read(dir.join("environ")) else { continue };
        if !env.split(|&b| b == 0).any(|var| LAUNCHER_VARS.iter().any(|m| var.starts_with(m))) {
            continue;
        }
        if let Ok(comm) = fs::read_to_string(dir.join("comm")) {
            names.insert(comm.trim_end().to_string());
        }
        if let Ok(cmdline) = fs::read(dir.join("cmdline")) {
            let argv0 = String::from_utf8_lossy(cmdline.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
            if let Some(file) = argv0.rsplit(['/', '\\']).next().filter(|f| !f.is_empty()) {
                names.insert(file.to_string());
            }
        }
    }
    names
}

/// MangoHud names logs after the program; `comm` holds only its first 15 characters.
fn is_game(games: &HashSet<String>, name: &str) -> bool {
    games.contains(name) || games.contains(&name.chars().take(15).collect::<String>())
}

/// Make sure the log folder exists (MangoHud won't create it, e.g. after the cache is cleared) and
/// delete logs of games that closed a while ago; they grow by ~15 KB per second of play.
pub fn maintain_log_dir() {
    let text = fs::read_to_string(mangohud_config()).unwrap_or_default();
    if text.contains(BLOCK_START) {
        // Set up by an older OpenCUE: bring the block up to date, since the user already chose it.
        if !current_block(&text) {
            match setup_mangohud() {
                Ok(path) => log::info!("updated OpenCUE's block in {}", path.display()),
                Err(e) => log::warn!("cannot update {}: {e}", mangohud_config().display()),
            }
        }
        let _ = fs::create_dir_all(log_dir());
    }
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
    fn picks_the_game_not_the_terminal() {
        let (game, term) = (
            PathBuf::from("/l/game.exe_2026-10-29_13-37-43.csv"),
            PathBuf::from("/l/vibeterm_2026-10-29_13-37-49.csv"),
        );
        let before = HashMap::from([(game.clone(), 50_000), (term.clone(), 90_000)]);
        let launched = |name: &str| name == "game.exe";
        let nothing = |_: &str| false;
        // Started by a launcher: chosen even while it isn't growing (MangoHud reloading its config).
        let now = [(game.clone(), 50_000), (term.clone(), 91_000)];
        assert_eq!(choose_log(&now, &before, 0.5, launched), Some(game.clone()));
        // Unknown programs: only one rendering at a game-like rate, and the faster one.
        let now = [(game.clone(), 58_000), (term.clone(), 90_400)];
        assert_eq!(choose_log(&now, &before, 0.5, nothing), Some(game.clone()));
        let now = [(term.clone(), 90_400)];
        assert_eq!(choose_log(&now, &before, 0.5, nothing), None);
        // A log seen for the first time has no rate yet.
        assert_eq!(choose_log(&[(PathBuf::from("/l/new_x_y.csv"), 9_000)], &before, 0.5, nothing), None);
        assert_eq!(choose_log(&[], &before, 0.5, launched), None);
    }

    #[test]
    fn matches_cut_down_process_names() {
        let games = HashSet::from(["Cyberpunk2077.e".to_string(), "Balatro.exe".to_string()]);
        assert!(is_game(&games, "Cyberpunk2077.exe"));
        assert!(is_game(&games, "Balatro.exe"));
        assert!(!is_game(&games, "vibeterm"));
    }

    #[test]
    fn config_block_is_replaced_not_duplicated() {
        let user = "fps_limit=144\n";
        let once = format!("{}{}", without_block(user), block(Path::new("/c/opencue/mangohud"), false));
        let twice = format!("{}{}", without_block(&once), block(Path::new("/c/opencue/mangohud"), true));
        assert_ne!(once, twice);
        assert!(once.contains(HIDDEN) && !twice.contains(HIDDEN));
        assert!(current_block(&once.replace("/c/opencue/mangohud", &log_dir().display().to_string())));
        assert!(once.starts_with("fps_limit=144\n"));
        assert_eq!(without_block(&once), user);
        assert_eq!(without_block(&twice), user);
    }

    #[test]
    fn overlay_state_is_read_from_the_settings() {
        assert!(!block_shows_overlay(&block(Path::new("/x"), false)));
        assert!(block_shows_overlay(&block(Path::new("/x"), true)));
        let older = format!("{BLOCK_START}\n# The overlay is transparent.\nalpha=0\nbackground_alpha=0\n{BLOCK_END}\n");
        let oldest = format!("{BLOCK_START}\nno_display\nautostart_log=1\n{BLOCK_END}\n");
        assert!(!block_shows_overlay(&older) && !block_shows_overlay(&oldest));
        // The user's own settings outside the block don't count.
        assert!(block_shows_overlay(&format!("alpha=0\n{}", block(Path::new("/x"), true))));
        assert!(!block_shows_overlay("alpha=1\n"));
    }
}
