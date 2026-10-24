//! Minimal Linux hidraw access: discovery through sysfs, raw reports and feature reports.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum HidError {
    #[error("no permission for {path} ({name}); install the udev rule: sudo ./install.sh --udev")]
    Permission { path: String, name: String },
    #[error("{0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("{0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, HidError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidInfo {
    pub path: PathBuf,
    pub vid: u16,
    pub pid: u16,
    pub interface: i32,
    pub serial: String,
    pub name: String,
    /// Physical USB location, e.g. "3-6.2.4". Stable across reconnects.
    pub usb_path: String,
}

fn read_trim(p: &Path) -> String {
    fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// List hidraw nodes, optionally filtered by vendor id.
pub fn enumerate(vid: Option<u16>) -> Vec<HidInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else { return out };
    let mut nodes: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    nodes.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("hidraw"))
            .and_then(|n| n.parse::<u32>().ok())
    });
    for node in nodes {
        let Ok(hid_dev) = fs::canonicalize(node.join("device")) else { continue };
        let uevent = read_trim(&hid_dev.join("uevent"));
        let field = |key: &str| {
            uevent.lines().find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('='))).unwrap_or("").to_string()
        };
        let hid_id = field("HID_ID");
        let parts: Vec<_> = hid_id.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let (Ok(dvid), Ok(dpid)) = (u32::from_str_radix(parts[1], 16), u32::from_str_radix(parts[2], 16)) else {
            continue;
        };
        let (dvid, dpid) = (dvid as u16, dpid as u16);
        if vid.is_some_and(|v| v != dvid) {
            continue;
        }
        let usb_iface = hid_dev.parent().map(Path::to_path_buf).unwrap_or_default();
        let interface = i32::from_str_radix(&read_trim(&usb_iface.join("bInterfaceNumber")), 16).unwrap_or(-1);
        let usb_dev = usb_iface.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut serial = read_trim(&usb_dev.join("serial"));
        if serial.is_empty() {
            serial = field("HID_UNIQ");
        }
        out.push(HidInfo {
            path: PathBuf::from("/dev").join(node.file_name().unwrap()),
            vid: dvid,
            pid: dpid,
            interface,
            serial,
            name: field("HID_NAME"),
            usb_path: usb_dev.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string(),
        });
    }
    out
}

const fn ioc_rw(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | ((b'H' as u64) << 8) | nr
}

/// One opened hidraw node. `write` takes the report id as its first byte (0 when the
/// device has no numbered reports), exactly like hidapi.
pub struct HidDevice {
    pub info: HidInfo,
    file: File,
}

impl HidDevice {
    pub fn open(info: &HidInfo) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(&info.path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                HidError::Permission { path: info.path.display().to_string(), name: info.name.clone() }
            } else {
                HidError::Io(format!("open {}", info.path.display()), e)
            }
        })?;
        Ok(Self { info: info.clone(), file })
    }

    fn err(&self, what: &str, e: std::io::Error) -> HidError {
        HidError::Io(format!("{what} {}", self.info.path.display()), e)
    }

    pub fn write(&self, data: &[u8]) -> Result<usize> {
        (&self.file).write(data).map_err(|e| self.err("write", e))
    }

    /// Read one report. Returns an empty vec on timeout.
    pub fn read(&self, size: usize, timeout: Duration) -> Result<Vec<u8>> {
        let mut pfd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(self.err("poll", e));
        }
        if n == 0 {
            return Ok(Vec::new());
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(self.err("read", std::io::Error::from_raw_os_error(libc::ENODEV)));
        }
        let mut buf = vec![0u8; size];
        let n = (&self.file).read(&mut buf).map_err(|e| self.err("read", e))?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Discard input reports that are already queued.
    pub fn drain(&self, size: usize) -> Result<()> {
        while !self.read(size, Duration::ZERO)?.is_empty() {}
        Ok(())
    }

    pub fn send_feature_report(&self, data: &[u8]) -> Result<()> {
        let mut buf = data.to_vec();
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), ioc_rw(0x06, buf.len()) as _, buf.as_mut_ptr()) };
        if r < 0 {
            return Err(self.err("feature report to", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn get_feature_report(&self, report_id: u8, size: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; size];
        buf[0] = report_id;
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), ioc_rw(0x07, size) as _, buf.as_mut_ptr()) };
        if r < 0 {
            return Err(self.err("feature read from", std::io::Error::last_os_error()));
        }
        buf.truncate(r as usize);
        Ok(buf)
    }
}
