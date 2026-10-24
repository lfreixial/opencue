//! Things a Nexus button can do: key combos and text through a virtual uinput keyboard,
//! media keys, commands and URLs.

use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};

static KEYBOARD: Mutex<Option<VirtualDevice>> = Mutex::new(None);

/// Map a friendly key name ("ctrl", "a", "f5", "volume_up") to a key code.
pub fn key_code(name: &str) -> Option<KeyCode> {
    let n = name.trim().to_ascii_lowercase();
    let alias = match n.as_str() {
        "ctrl" | "control" | "lctrl" => "leftctrl",
        "rctrl" => "rightctrl",
        "shift" | "lshift" => "leftshift",
        "rshift" => "rightshift",
        "alt" | "lalt" => "leftalt",
        "altgr" | "ralt" => "rightalt",
        "super" | "meta" | "win" | "cmd" | "logo" => "leftmeta",
        "return" => "enter",
        "escape" => "esc",
        "del" => "delete",
        "ins" => "insert",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        "print" | "printscreen" | "prtsc" => "sysrq",
        "play_pause" | "playpause" => "playpause",
        "next" => "nextsong",
        "previous" | "prev" => "previoussong",
        "stop" => "stopcd",
        "volume_up" => "volumeup",
        "volume_down" => "volumedown",
        "mute" => "mute",
        "-" => "minus",
        "=" => "equal",
        "[" => "leftbrace",
        "]" => "rightbrace",
        ";" => "semicolon",
        "'" => "apostrophe",
        "`" => "grave",
        "\\" => "backslash",
        "," => "comma",
        "." => "dot",
        "/" => "slash",
        " " => "space",
        other => other,
    };
    KeyCode::from_str(&format!("KEY_{}", alias.to_ascii_uppercase())).ok()
}

/// Parse "ctrl+shift+t" into the keys to hold, in order.
pub fn parse_combo(combo: &str) -> Result<Vec<KeyCode>> {
    let parts: Vec<&str> = if combo.trim() == "+" { vec!["+"] } else { combo.split('+').collect() };
    parts.iter().map(|p| key_code(p).ok_or_else(|| anyhow!("unknown key '{p}' in '{combo}'"))).collect()
}

/// US-layout key + whether shift is needed for one character.
fn char_key(c: char) -> Option<(KeyCode, bool)> {
    const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";
    const PLAIN: &str = "`1234567890-=[]\\;',./";
    if c.is_ascii_uppercase() {
        return key_code(&c.to_ascii_lowercase().to_string()).map(|k| (k, true));
    }
    if let Some(i) = SHIFTED.find(c) {
        return key_code(&PLAIN[i..i + 1]).map(|k| (k, true));
    }
    match c {
        '\n' => Some((KeyCode::KEY_ENTER, false)),
        '\t' => Some((KeyCode::KEY_TAB, false)),
        _ => key_code(&c.to_string()).map(|k| (k, false)),
    }
}

fn with_keyboard<T>(f: impl FnOnce(&mut VirtualDevice) -> Result<T>) -> Result<T> {
    let mut guard = KEYBOARD.lock().unwrap();
    if guard.is_none() {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=0x2ff_u16 {
            keys.insert(KeyCode::new(code));
        }
        let dev = VirtualDevice::builder()
            .context("cannot open /dev/uinput (is the opencue udev rule installed?)")?
            .name("OpenCUE virtual keyboard")
            .with_keys(&keys)?
            .build()?;
        *guard = Some(dev);
        // Give the compositor a moment to pick up the new device before the first key press.
        thread::sleep(Duration::from_millis(300));
    }
    f(guard.as_mut().unwrap())
}

fn key_event(code: KeyCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, value)
}

pub fn press_combo(keys: &[KeyCode]) -> Result<()> {
    with_keyboard(|dev| {
        for k in keys {
            dev.emit(&[key_event(*k, 1)])?;
        }
        thread::sleep(Duration::from_millis(20));
        for k in keys.iter().rev() {
            dev.emit(&[key_event(*k, 0)])?;
        }
        Ok(())
    })
}

pub fn type_text(text: &str) -> Result<()> {
    with_keyboard(|dev| {
        for c in text.chars() {
            let Some((key, shift)) = char_key(c) else {
                log::warn!("cannot type '{c}' (only US-layout ASCII is supported)");
                continue;
            };
            if shift {
                dev.emit(&[key_event(KeyCode::KEY_LEFTSHIFT, 1)])?;
            }
            dev.emit(&[key_event(key, 1)])?;
            dev.emit(&[key_event(key, 0)])?;
            if shift {
                dev.emit(&[key_event(KeyCode::KEY_LEFTSHIFT, 0)])?;
            }
            thread::sleep(Duration::from_millis(8));
        }
        Ok(())
    })
}

/// Run a program detached from the daemon, reaping it when it exits.
pub fn spawn(program: &str, args: &[&str]) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn run_command(cmd: &str) -> Result<()> {
    spawn("sh", &["-c", cmd])
}

pub fn open_url(url: &str) -> Result<()> {
    spawn("xdg-open", &[url])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_combos() {
        assert_eq!(
            parse_combo("ctrl+shift+t").unwrap(),
            vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_T]
        );
        assert_eq!(parse_combo("super+f5").unwrap(), vec![KeyCode::KEY_LEFTMETA, KeyCode::KEY_F5]);
        assert_eq!(parse_combo("play_pause").unwrap(), vec![KeyCode::KEY_PLAYPAUSE]);
        assert!(parse_combo("ctrl+bogus").is_err());
    }

    #[test]
    fn maps_characters() {
        assert_eq!(char_key('A'), Some((KeyCode::KEY_A, true)));
        assert_eq!(char_key('!'), Some((KeyCode::KEY_1, true)));
        assert_eq!(char_key('/'), Some((KeyCode::KEY_SLASH, false)));
    }
}
