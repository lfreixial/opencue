//! Monitor capture through the desktop portal and a small GStreamer pipeline. The daemon owns
//! the session so capture continues when the GUI closes. All pixels stay in memory.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_lite::{
    StreamExt, future,
    io::{AsyncRead, AsyncReadExt},
};
use opencue_core::ambilight::{CaptureState, FPS, HEIGHT, ScreenFrame, WIDTH};
use opencue_core::config::Effect;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

use super::Shared;

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
static TOKEN: AtomicU64 = AtomicU64::new(0);
type Results = HashMap<String, OwnedValue>;

fn token() -> String {
    format!("opencue_{}_{}", std::process::id(), TOKEN.fetch_add(1, Ordering::Relaxed))
}

/// Subscribe before the method call, since a portal can respond before the call returns.
async fn request(
    conn: &Connection,
    method: &str,
    session: Option<&OwnedObjectPath>,
    mut options: HashMap<&str, Value<'_>>,
    cancel: &AtomicBool,
) -> Result<Results> {
    let handle_token = token();
    let sender = conn
        .unique_name()
        .context("session bus has no unique name")?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("{PATH}/request/{sender}/{handle_token}");
    let req = Proxy::new(conn, DEST, path.as_str(), "org.freedesktop.portal.Request").await?;
    let mut signals = req.receive_signal("Response").await?;
    options.insert("handle_token", Value::from(handle_token.as_str()));
    let portal = Proxy::new(conn, DEST, PATH, "org.freedesktop.portal.ScreenCast").await?;
    let operation = async {
        let _: OwnedObjectPath = match session {
            None => portal.call(method, &(options,)).await?,
            Some(s) if method == "Start" => portal.call(method, &(s, "", options)).await?,
            Some(s) => portal.call(method, &(s, options)).await?,
        };
        let msg = signals.next().await.context("screen-sharing portal disconnected")?;
        let (code, results): (u32, Results) = msg.body().deserialize()?;
        match code {
            0 => Ok(results),
            1 => bail!("Screen sharing was cancelled. Choose a monitor to try again."),
            _ => bail!("The desktop could not start screen sharing. Check your desktop portal backend."),
        }
    };
    let cancelled = async {
        while !cancel.load(Ordering::Relaxed) {
            async_io::Timer::after(Duration::from_millis(100)).await;
        }
        Err(anyhow!("Screen sharing stopped."))
    };
    let result = future::or(operation, cancelled).await;
    if cancel.load(Ordering::Relaxed) {
        let _ = req.call::<_, _, ()>("Close", &()).await;
    }
    result
}

struct Session {
    conn: zbus::blocking::Connection,
    path: OwnedObjectPath,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.conn.call_method(Some(DEST), &self.path, Some("org.freedesktop.portal.Session"), "Close", &());
    }
}

fn stream_target(node: u32, properties: &Results) -> String {
    properties
        .get("pipewire-serial")
        .and_then(|v| u64::try_from(v).ok())
        .map_or_else(|| format!("path={node}"), |serial| format!("target-object={serial}"))
}

async fn open(conn: Connection, cancel: &AtomicBool) -> Result<(Session, OwnedFd, String)> {
    let session_token = token();
    let mut created = request(
        &conn,
        "CreateSession",
        None,
        HashMap::from([("session_handle_token", Value::from(session_token.as_str()))]),
        cancel,
    )
    .await?;
    let path = String::try_from(created.remove("session_handle").context("portal returned no session")?)?;
    let session = Session { conn: conn.clone().into(), path: OwnedObjectPath::try_from(path)? };
    request(
        &conn,
        "SelectSources",
        Some(&session.path),
        HashMap::from([("types", Value::from(1u32)), ("multiple", Value::from(false))]),
        cancel,
    )
    .await?;
    let mut started = request(&conn, "Start", Some(&session.path), HashMap::new(), cancel).await?;
    let streams: Vec<(u32, Results)> = started.remove("streams").context("portal returned no monitor")?.try_into()?;
    let (node, properties) = streams.first().context("No monitor was selected")?;
    // Newer portals supply a stable PipeWire serial; older backends use the node id.
    let target = stream_target(*node, properties);
    let proxy = Proxy::new(&conn, DEST, PATH, "org.freedesktop.portal.ScreenCast").await?;
    let fd: zbus::zvariant::OwnedFd =
        proxy.call("OpenPipeWireRemote", &(&session.path, HashMap::<&str, Value<'_>>::new())).await?;
    Ok((session, fd.into(), target))
}

struct Pipeline(Child);

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn video_filters() -> Vec<String> {
    [
        "!",
        &format!("video/x-raw,max-framerate={FPS}/1"),
        "!",
        "videorate",
        "drop-only=true",
        &format!("max-rate={FPS}"),
        "!",
        &format!("video/x-raw,framerate={FPS}/1"),
        // If the reader falls behind, keep the newest image instead of building video latency.
        "!",
        "queue",
        "max-size-buffers=1",
        "max-size-bytes=0",
        "max-size-time=0",
        "leaky=downstream",
        "!",
        "videoconvertscale",
        "add-borders=false",
        "method=0",
        "dither=none",
        "!",
        &format!("video/x-raw,format=RGB,width={WIDTH},height={HEIGHT},pixel-aspect-ratio=1/1"),
        "!",
        "fdsink",
        "fd=1",
        "sync=false",
        "enable-last-sample=false",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn pipeline(fd: &OwnedFd, target: &str) -> Result<Pipeline> {
    let raw = fd.as_raw_fd();
    let mut command = Command::new("gst-launch-1.0");
    command
        .args(["-q", "pipewiresrc", "fd=3", target, "do-timestamp=true", "max-buffers=3"])
        .args(video_filters())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Only the child inherits the portal's PipeWire remote. dup2 also clears CLOEXEC.
    unsafe {
        command.pre_exec(move || {
            if raw == 3 {
                if libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(raw, 3) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(Pipeline(
        command.spawn().context(
            "Install GStreamer tools, base plugins and the PipeWire plugin to use Ambilight (gst-launch-1.0)",
        )?,
    ))
}

async fn read_frames(
    reader: impl AsyncRead + Unpin,
    cancel: &AtomicBool,
    mut publish: impl FnMut(ScreenFrame),
) -> Result<()> {
    let received = std::cell::Cell::new(false);
    let frames = async {
        let mut reader = reader;
        let mut bytes = vec![0u8; WIDTH * HEIGHT * 3];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            reader.read_exact(&mut bytes).await.context("Monitor video stream ended")?;
            received.set(true);
            publish(ScreenFrame { pixels: bytes.as_chunks::<3>().0.to_vec() });
        }
    };
    let stopped = async {
        while !cancel.load(Ordering::Relaxed) {
            async_io::Timer::after(Duration::from_millis(100)).await;
        }
        Ok(())
    };
    let startup_timeout = async {
        async_io::Timer::after(Duration::from_secs(10)).await;
        if !received.get() {
            bail!("The monitor did not provide video frames. Choose the monitor again.");
        }
        future::pending::<Result<()>>().await
    };
    // The reactor wakes on readable bytes. A frame split across pipe writes incurs no sleeps.
    future::or(frames, future::or(stopped, startup_timeout)).await
}

fn capture(sh: &Shared, cancel: &AtomicBool) -> Result<()> {
    // Check before showing the monitor picker so missing dependencies are actionable immediately.
    let status = Command::new("gst-inspect-1.0")
        .arg("pipewiresrc")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Install GStreamer tools, base plugins and the PipeWire plugin to use Ambilight")?;
    if !status.success() {
        bail!(
            "The GStreamer PipeWire plugin is missing. Install gstreamer1.0-pipewire (Debian/Ubuntu) or pipewire-gstreamer (Arch)."
        )
    }
    let conn = future::block_on(Connection::session()).context("Cannot connect to the desktop session bus")?;
    let (session, fd, target) = future::block_on(open(conn, cancel))?;
    sh.telemetry(|t| t.ambilight.status = "Starting monitor capture…".into());
    let session_proxy =
        future::block_on(Proxy::new(session.conn.inner(), DEST, &session.path, "org.freedesktop.portal.Session"))?;
    let mut closed = future::block_on(session_proxy.receive_signal("Closed"))?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut child = pipeline(&fd, &target)?;
    let stdout = async_io::Async::new(child.0.stdout.take().context("capture has no output pipe")?)?;
    let stderr = child.0.stderr.take().context("capture has no error pipe")?;
    let errors = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.take(8192).read_to_string(&mut text);
        text
    });
    let result =
        future::block_on(future::or(read_frames(stdout, cancel, |frame| sh.set_ambilight_frame(frame)), async {
            closed.next().await;
            Err(anyhow!("Screen sharing was ended by the desktop"))
        }));
    drop(child);
    let error = errors.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    bail!("{}. Choose a monitor to reconnect. {}", result.unwrap_err(), error.trim())
}

pub(super) fn run(sh: Arc<Shared>, rx: mpsc::Receiver<bool>) {
    let mut task: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)> = None;
    while !sh.stopping() {
        let command = rx.recv_timeout(Duration::from_millis(100)).ok();
        let needed = sh.config.read().unwrap().profile().lighting.values().any(|l| l.effect == Effect::Ambilight);
        if (command.is_some() || !needed)
            && let Some((cancel, handle)) = task.take()
        {
            cancel.store(true, Ordering::Relaxed);
            let _ = handle.join();
            sh.set_ambilight(CaptureState::default());
        }
        if command == Some(true) && needed {
            sh.set_ambilight(CaptureState {
                active: true,
                status: "Choose a monitor in the desktop screen-sharing dialog…".into(),
                frame: None,
                ..Default::default()
            });
            let cancel = Arc::new(AtomicBool::new(false));
            let (worker_sh, worker_cancel) = (sh.clone(), cancel.clone());
            let handle = thread::spawn(move || {
                if let Err(e) = capture(&worker_sh, &worker_cancel) {
                    worker_sh.set_ambilight(CaptureState {
                        active: false,
                        status: format!("{e:#}"),
                        frame: None,
                        ..Default::default()
                    });
                }
            });
            task = Some((cancel, handle));
        }
    }
    if let Some((cancel, handle)) = task {
        cancel.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    const SESSION: &str = "/org/freedesktop/portal/desktop/session/test";

    #[test]
    fn reader_handles_fragmented_frames_and_idle_cancellation() {
        let (socket, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let producer = thread::spawn(move || {
            for color in [[255, 0, 0], [0, 0, 255]] {
                let pixels = color.repeat(WIDTH * HEIGHT);
                for chunk in pixels.chunks(511) {
                    std::io::Write::write_all(&mut writer, chunk).unwrap();
                    thread::yield_now();
                }
            }
        });
        let cancel = AtomicBool::new(false);
        let mut frames = Vec::new();
        future::block_on(read_frames(async_io::Async::new(socket).unwrap(), &cancel, |frame| {
            frames.push(frame);
            if frames.len() == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        }))
        .unwrap();
        producer.join().unwrap();
        assert_eq!(frames[0].pixels, vec![[255, 0, 0]; WIDTH * HEIGHT]);
        assert_eq!(frames[1].pixels, vec![[0, 0, 255]; WIDTH * HEIGHT]);

        let (socket, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let timer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            worker_cancel.store(true, Ordering::Relaxed);
        });
        future::block_on(read_frames(async_io::Async::new(socket).unwrap(), &cancel, |_| {
            panic!("idle stream produced a frame")
        }))
        .unwrap();
        timer.join().unwrap();
    }

    /// Measures the real video filters and reader without needing a monitor or RGB devices.
    #[test]
    #[ignore = "manual GStreamer throughput benchmark"]
    fn capture_throughput() {
        let mut child = Pipeline(
            Command::new("gst-launch-1.0")
                .args([
                    "-q",
                    "videotestsrc",
                    "is-live=true",
                    "pattern=red",
                    "!",
                    "video/x-raw,format=BGRx,width=3840,height=2160,framerate=144/1",
                    "!",
                    "identity",
                ])
                .args(video_filters())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = async_io::Async::new(child.0.stdout.take().unwrap()).unwrap();
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let mut frames = 0;
        future::block_on(read_frames(stdout, &cancel, |frame| {
            frames += 1;
            assert!(frame.pixels.iter().all(|&p| p == [255, 0, 0]));
            if started.elapsed() >= Duration::from_secs(3) {
                cancel.store(true, Ordering::Relaxed);
            }
        }))
        .unwrap();
        let fps = frames as f64 / started.elapsed().as_secs_f64();
        eprintln!("Capture: {fps:.1} fps from a 4K / 144 Hz source, {} bytes/frame", WIDTH * HEIGHT * 3);
        assert!(fps >= 50.0, "capture should keep up with a 60 Hz target: {fps:.1} fps");
    }

    struct TestPortal {
        wait_for_picker: Arc<AtomicBool>,
        picker_open: Arc<AtomicBool>,
    }

    async fn respond(
        conn: &Connection,
        header: &zbus::message::Header<'_>,
        options: Results,
        results: Results,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let sender = header.sender().unwrap();
        let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
        let path = format!("{PATH}/request/{}/{token}", sender.as_str().trim_start_matches(':').replace('.', "_"));
        // Deliberately emit before the method returns to exercise the subscription race.
        conn.emit_signal(
            Some(sender.as_str()),
            path.as_str(),
            "org.freedesktop.portal.Request",
            "Response",
            &(0u32, results),
        )
        .await?;
        Ok(OwnedObjectPath::try_from(path).unwrap())
    }

    #[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
    impl TestPortal {
        async fn create_session(
            &self,
            options: Results,
            #[zbus(connection)] conn: &Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            respond(
                conn,
                &header,
                options,
                HashMap::from([("session_handle".into(), Value::from(SESSION).try_into().unwrap())]),
            )
            .await
        }

        async fn select_sources(
            &self,
            session: OwnedObjectPath,
            options: Results,
            #[zbus(connection)] conn: &Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            assert_eq!(session.as_str(), SESSION);
            assert_eq!(u32::try_from(options.get("types").unwrap()).unwrap(), 1);
            assert!(!bool::try_from(options.get("multiple").unwrap()).unwrap());
            respond(conn, &header, options, HashMap::new()).await
        }

        async fn start(
            &self,
            session: OwnedObjectPath,
            parent: String,
            options: Results,
            #[zbus(connection)] conn: &Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            assert_eq!(session.as_str(), SESSION);
            assert!(parent.is_empty());
            if self.wait_for_picker.load(Ordering::Relaxed) {
                self.picker_open.store(true, Ordering::Relaxed);
                return Ok(OwnedObjectPath::try_from("/pending").unwrap());
            }
            let streams = vec![(42u32, HashMap::from([("pipewire-serial".to_string(), OwnedValue::from(1234u64))]))];
            respond(
                conn,
                &header,
                options,
                HashMap::from([("streams".into(), Value::from(streams).try_into().unwrap())]),
            )
            .await
        }

        fn open_pipe_wire_remote(
            &self,
            session: OwnedObjectPath,
            options: Results,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
            assert_eq!(session.as_str(), SESSION);
            assert!(options.is_empty());
            let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
            let fd: OwnedFd = socket.into();
            Ok(fd.into())
        }
    }

    struct TestSession(Arc<AtomicBool>);

    #[zbus::interface(name = "org.freedesktop.portal.Session")]
    impl TestSession {
        fn close(&self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn portal_lifecycle_and_picker_cancellation() {
        let Ok(child) = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
        else {
            eprintln!("skipping portal integration test: dbus-daemon is unavailable");
            return;
        };
        let mut bus = Pipeline(child);
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let wait_for_picker = Arc::new(AtomicBool::new(false));
        let picker_open = Arc::new(AtomicBool::new(false));
        let _server = future::block_on(
            zbus::connection::Builder::address(address.trim())
                .unwrap()
                .name(DEST)
                .unwrap()
                .serve_at(
                    PATH,
                    TestPortal { wait_for_picker: wait_for_picker.clone(), picker_open: picker_open.clone() },
                )
                .unwrap()
                .serve_at(SESSION, TestSession(closed.clone()))
                .unwrap()
                .build(),
        )
        .unwrap();
        let client = future::block_on(zbus::connection::Builder::address(address.trim()).unwrap().build()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (session, fd, target) = future::block_on(open(client.clone(), &cancel)).unwrap();
        assert!(fd.as_raw_fd() >= 0);
        assert_eq!(target, "target-object=1234");
        drop(session);
        assert!(closed.load(Ordering::Relaxed));
        assert_eq!(stream_target(42, &HashMap::new()), "path=42");

        let worker_cancel = cancel.clone();
        closed.store(false, Ordering::Relaxed);
        wait_for_picker.store(true, Ordering::Relaxed);
        let worker = thread::spawn(move || future::block_on(open(client, &worker_cancel)));
        let started = std::time::Instant::now();
        while !picker_open.load(Ordering::Relaxed) {
            assert!(started.elapsed() < Duration::from_secs(5), "picker must open");
            thread::sleep(Duration::from_millis(10));
        }
        cancel.store(true, Ordering::Relaxed);
        assert!(worker.join().unwrap().err().unwrap().to_string().contains("stopped"));
        assert!(closed.load(Ordering::Relaxed), "cancelling the picker must close its session");
    }
}
