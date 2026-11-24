//! Device session orchestration.
//!
//! Threads:
//!  * controller  – owns adb calls (device polling, orientation, fallback keys) and the connection.
//!  * connection  – prepares the device, connects the forwarded socket, then runs the read loop.
//!  * writer      – serialises `HostMsg`s (touch/keys) to the socket.
//!  * decoder     – H.264 -> BGRA; publishes the newest frame to `Shared`.
//!
//! The GPUI view only reads `Shared` and sends input through `Shared::send_input`.

use crate::audio::AudioPlayer;
use crate::decoder::H264;
use crate::recorder::Recorder;
use rd_adbkit::{Adb, Device, OrientationMode};
use rd_protocol::{self as proto, AudioStatus, DeviceMsg, HostMsg};
use std::collections::VecDeque;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const MAX_SIZE: u32 = 1920;
pub const BITRATE: u32 = 8_000_000;

#[derive(Clone, PartialEq, Debug)]
pub enum Phase {
    Idle,
    Preparing(String),
    WaitingForDevice,
    Streaming,
    Error(String),
}

#[derive(Clone)]
pub struct State {
    pub devices: Vec<Device>,
    pub selected: Option<String>,
    pub phase: Phase,
    pub device_name: String,
    /// Size of the (view-rotated) picture currently shown.
    pub frame_size: (u32, u32),
    pub audio: AudioStatus,
    pub input_ready: bool,
    pub recording: bool,
    pub note: String,
    pub fps: u32,
}

pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Where new files are first written, and the folder the Save As dialog opens in. Updated to
    /// the folder you last saved into.
    pub recordings_dir: PathBuf,
    pub screenshots_dir: PathBuf,
}

impl Settings {
    fn default_dir() -> PathBuf {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| ".".into()).join("RemoteDisplay")
    }

    fn file() -> PathBuf {
        let base = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else if cfg!(target_os = "macos") {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
        } else {
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        };
        base.unwrap_or_else(|| ".".into()).join("remote-display").join("settings.json")
    }

    pub fn load() -> Self {
        let d = Self::default_dir();
        let mut s = Settings { recordings_dir: d.clone(), screenshots_dir: d };
        if let Ok(text) = std::fs::read_to_string(Self::file()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(p) = v["recordings_dir"].as_str().filter(|p| !p.is_empty()) {
                    s.recordings_dir = p.into();
                }
                if let Some(p) = v["screenshots_dir"].as_str().filter(|p| !p.is_empty()) {
                    s.screenshots_dir = p.into();
                }
            }
        }
        s
    }

    pub fn save(&self) {
        let f = Self::file();
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let v = serde_json::json!({
            "recordings_dir": self.recordings_dir.to_string_lossy(),
            "screenshots_dir": self.screenshots_dir.to_string_lossy(),
        });
        let _ = std::fs::write(f, serde_json::to_string_pretty(&v).unwrap_or_default());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveKind {
    Recording,
    Screenshot,
}

/// A finished file the UI may offer to relocate via a Save-As dialog.
#[derive(Clone, Debug)]
pub struct SaveRequest {
    pub path: PathBuf,
    pub kind: SaveKind,
}

/// `YYYYMMDD-HHMMSS` in UTC (no timezone database needed).
pub fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub struct Shared {
    pub state: Mutex<State>,
    pub frame: Mutex<Option<VideoFrame>>,
    pub dirty: AtomicBool,
    pub view_rotation: AtomicU8,
    pub muted: AtomicBool,
    input_tx: Mutex<Option<Sender<HostMsg>>>,
    pub settings: Mutex<Settings>,
    pub save_requests: Mutex<VecDeque<SaveRequest>>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                devices: vec![],
                selected: None,
                phase: Phase::Idle,
                device_name: String::new(),
                frame_size: (0, 0),
                audio: AudioStatus::Unknown,
                input_ready: false,
                recording: false,
                note: String::new(),
                fps: 0,
            }),
            frame: Mutex::new(None),
            dirty: AtomicBool::new(true),
            view_rotation: AtomicU8::new(0),
            muted: AtomicBool::new(false),
            input_tx: Mutex::new(None),
            settings: Mutex::new(Settings::load()),
            save_requests: Mutex::new(VecDeque::new()),
        })
    }

    pub fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
        self.dirty.store(true, Ordering::Release);
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut g = self.settings.lock().unwrap();
        f(&mut g);
        g.save();
        drop(g);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn send_input(&self, msg: HostMsg) {
        if let Some(tx) = self.input_tx.lock().unwrap().as_ref() {
            let _ = tx.send(msg);
        }
    }
}

pub enum Cmd {
    Refresh,
    Select(String),
    Connect,
    Disconnect,
    Orientation(OrientationMode),
    /// `prompt`: offer a Save As dialog when the recording is finalised (UI-initiated only).
    Record { on: bool, prompt: bool },
    KeyFallback(i32),
    Screenshot,
    /// Stop everything (finalising any recording), acknowledge, and end the controller.
    Shutdown(Sender<()>),
}

#[derive(Clone)]
pub struct Controller {
    tx: Sender<Cmd>,
    pub shared: Arc<Shared>,
}

impl Controller {
    pub fn spawn() -> Controller {
        let shared = Shared::new();
        let (tx, rx) = channel();
        let s = shared.clone();
        std::thread::Builder::new().name("controller".into()).spawn(move || controller(s, rx)).unwrap();
        let _ = tx.send(Cmd::Refresh);
        Controller { tx, shared }
    }
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }

    /// Disconnect and wait (bounded) until any recording is finalised on disk. Call before exit.
    pub fn shutdown(&self) {
        let (ack_tx, ack_rx) = channel();
        if self.tx.send(Cmd::Shutdown(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(60));
        }
    }
}

struct Conn {
    stop: Arc<AtomicBool>,
    sock: Arc<Mutex<Option<TcpStream>>>,
    rec: Arc<Mutex<Option<Recorder>>>,
    rec_init: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Conn {
    fn shutdown(self) {
        self.stop.store(true, Ordering::Release);
        if let Some(s) = self.sock.lock().unwrap().as_ref() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        let _ = self.handle.join();
    }
}

fn controller(shared: Arc<Shared>, rx: Receiver<Cmd>) {
    let adb = match Adb::locate() {
        Ok(a) => {
            let _ = a.start_server();
            a
        }
        Err(e) => {
            shared.update(|s| s.phase = Phase::Error(e.to_string()));
            return;
        }
    };
    let mut conn: Option<Conn> = None;

    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Cmd::Refresh) | Err(RecvTimeoutError::Timeout) => refresh_devices(&adb, &shared),
            Ok(Cmd::Select(serial)) => shared.update(|s| s.selected = Some(serial)),
            Ok(Cmd::Connect) => {
                if let Some(c) = conn.take() {
                    c.shutdown();
                }
                if let Some(serial) = shared.snapshot().selected {
                    conn = Some(start_conn(adb.clone(), shared.clone(), serial));
                }
            }
            Ok(Cmd::Disconnect) => {
                if let Some(c) = conn.take() {
                    stop_recording(&c, &shared, true);
                    c.shutdown();
                }
                shared.update(|s| {
                    s.phase = Phase::Idle;
                    s.input_ready = false;
                });
            }
            Ok(Cmd::Orientation(mode)) => {
                if let Some(serial) = shared.snapshot().selected {
                    if let Err(e) = adb.set_orientation(&serial, mode) {
                        shared.update(|s| s.note = e.to_string());
                    }
                }
            }
            Ok(Cmd::Record { on, prompt }) => {
                if let Some(c) = &conn {
                    if on {
                        let dir = shared.settings().recordings_dir;
                        let stem = format!("session-{}", utc_stamp());
                        match Recorder::start(&dir, &stem) {
                            Ok(r) => {
                                *c.rec.lock().unwrap() = Some(r);
                                c.rec_init.store(true, Ordering::Release);
                                shared.send_input(HostMsg::RequestKeyframe);
                                shared.update(|s| {
                                    s.recording = true;
                                    s.note = format!("recording to {}", dir.display());
                                });
                            }
                            Err(e) => shared.update(|s| s.note = format!("cannot record: {e}")),
                        }
                    } else {
                        stop_recording(c, &shared, prompt);
                    }
                }
            }
            Ok(Cmd::Screenshot) => take_screenshot(&adb, &shared),
            Ok(Cmd::KeyFallback(code)) => {
                if let Some(serial) = shared.snapshot().selected {
                    adb.keyevent(&serial, code);
                }
            }
            Ok(Cmd::Shutdown(ack)) => {
                if let Some(c) = conn.take() {
                    stop_recording(&c, &shared, false);
                    c.shutdown();
                }
                let _ = ack.send(());
                return;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if conn.as_ref().is_some_and(|c| c.handle.is_finished()) {
            let c = conn.take().unwrap();
            stop_recording(&c, &shared, true);
            c.shutdown();
        }
    }
    if let Some(c) = conn {
        c.shutdown();
    }
}

fn stop_recording(c: &Conn, shared: &Shared, prompt: bool) {
    if let Some(r) = c.rec.lock().unwrap().take() {
        let path = r.path.clone();
        let msg = r.stop();
        if prompt && path.exists() {
            shared.save_requests.lock().unwrap().push_back(SaveRequest { path, kind: SaveKind::Recording });
        }
        shared.update(|s| {
            s.recording = false;
            s.note = msg;
        });
    }
}

fn take_screenshot(adb: &Adb, shared: &Shared) {
    let Some(serial) = shared.snapshot().selected else {
        shared.update(|s| s.note = "No device selected for the screenshot".into());
        return;
    };
    let settings = shared.settings();
    let result = adb.screencap(&serial).map_err(|e| e.to_string()).and_then(|png| {
        std::fs::create_dir_all(&settings.screenshots_dir).map_err(|e| format!("cannot create {}: {e}", settings.screenshots_dir.display()))?;
        let path = unique_path(&settings.screenshots_dir, &format!("screenshot-{}", utc_stamp()), "png");
        std::fs::write(&path, png).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(path)
    });
    match result {
        Ok(path) => {
            shared.update(|s| s.note = format!("Screenshot saved: {}", path.display()));
            shared.save_requests.lock().unwrap().push_back(SaveRequest { path, kind: SaveKind::Screenshot });
        }
        Err(e) => shared.update(|s| s.note = format!("Screenshot failed: {e}")),
    }
}

/// `dir/stem.ext`, or `dir/stem-2.ext`, ... if that exists.
fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem}-{n}.{ext}"));
        n += 1;
    }
    p
}

fn refresh_devices(adb: &Adb, shared: &Shared) {
    match adb.devices() {
        Ok(list) => shared.update(|s| {
            if s.devices != list {
                s.devices = list;
            }
            let valid = s.selected.as_ref().is_some_and(|sel| s.devices.iter().any(|d| &d.serial == sel));
            if !valid {
                s.selected = s.devices.iter().find(|d| d.ready()).map(|d| d.serial.clone());
            }
        }),
        Err(e) => shared.update(|s| s.note = e.to_string()),
    }
}

fn start_conn(adb: Adb, shared: Arc<Shared>, serial: String) -> Conn {
    let stop = Arc::new(AtomicBool::new(false));
    let sock = Arc::new(Mutex::new(None));
    let rec = Arc::new(Mutex::new(None));
    let rec_init = Arc::new(AtomicBool::new(false));
    let ctx = ConnCtx { adb, shared: shared.clone(), serial, stop: stop.clone(), sock: sock.clone(), rec: rec.clone(), rec_init: rec_init.clone() };
    let handle = std::thread::Builder::new()
        .name("connection".into())
        .spawn(move || {
            let shared = ctx.shared.clone();
            let result = run_conn(&ctx);
            *shared.input_tx.lock().unwrap() = None;
            ctx.adb_cleanup();
            // The stream is over (decoder already joined): never leave the last picture behind.
            *shared.frame.lock().unwrap() = None;
            shared.update(|s| {
                s.input_ready = false;
                s.audio = AudioStatus::Unknown;
                s.frame_size = (0, 0);
                s.fps = 0;
                s.device_name.clear();
                s.phase = match result {
                    Ok(()) => Phase::Idle,
                    Err(e) if ctx.stop.load(Ordering::Acquire) => {
                        let _ = e;
                        Phase::Idle
                    }
                    Err(e) => Phase::Error(e),
                };
            });
        })
        .unwrap();
    Conn { stop, sock, rec, rec_init, handle }
}

struct ConnCtx {
    adb: Adb,
    shared: Arc<Shared>,
    serial: String,
    stop: Arc<AtomicBool>,
    sock: Arc<Mutex<Option<TcpStream>>>,
    rec: Arc<Mutex<Option<Recorder>>>,
    rec_init: Arc<AtomicBool>,
}

impl ConnCtx {
    fn adb_cleanup(&self) {
        *self.sock.lock().unwrap() = None;
    }
    fn phase(&self, p: Phase) {
        self.shared.update(|s| s.phase = p);
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// Try to connect to the forwarded socket and read the device Hello.
fn try_hello(port: u16, wait: Duration) -> Option<(TcpStream, DeviceMsg)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    let _ = s.set_nodelay(true);
    s.set_read_timeout(Some(wait)).ok()?;
    let frame = proto::read_frame(&mut s).ok()?;
    let msg = DeviceMsg::parse(frame)?;
    s.set_read_timeout(None).ok()?;
    matches!(msg, DeviceMsg::Hello { .. }).then_some((s, msg))
}

fn run_conn(c: &ConnCtx) -> Result<(), String> {
    let adb = &c.adb;
    let serial = c.serial.as_str();
    c.phase(Phase::Preparing("forwarding port".into()));
    let port = adb.forward(serial, 0).map_err(|e| e.to_string())?;

    // Fast path: companion service already running.
    let mut hello = try_hello(port, Duration::from_millis(800));
    // A running service from an older companion build can't be reused: stop it so the
    // install/launch path below replaces it.
    if let Some((_, DeviceMsg::Hello { version_code, .. })) = &hello {
        if *version_code < proto::COMPANION_VERSION_CODE {
            hello = None;
            adb.stop_app(serial);
            c.shared.update(|s| s.note = "companion app is outdated; updating".into());
        }
    }

    if hello.is_none() {
        c.phase(Phase::Preparing("checking companion app".into()));
        let apk = rd_adbkit::find_apk();
        let installed = adb.ensure_installed(serial, apk.as_deref()).map_err(|e| e.to_string())?;
        if installed {
            c.shared.update(|s| s.note = "companion app installed".into());
        }
        adb.prepare_permissions(serial);
        c.phase(Phase::Preparing("launching companion app".into()));
        adb.launch(serial, MAX_SIZE, BITRATE).map_err(|e| e.to_string())?;
        c.phase(Phase::WaitingForDevice);
        c.shared.update(|s| s.note = "Accept the screen-capture prompt on the device".into());

        let deadline = Instant::now() + Duration::from_secs(120);
        while hello.is_none() {
            if c.stopped() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err("timed out waiting for the device (was the capture prompt accepted?)".into());
            }
            hello = try_hello(port, Duration::from_millis(700));
            if hello.is_none() {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    let (stream, hello) = hello.unwrap();
    let DeviceMsg::Hello { version, version_code, sdk, name } = hello else { unreachable!() };
    if version != proto::PROTOCOL_VERSION {
        return Err(format!("protocol mismatch: device {version}, host {}", proto::PROTOCOL_VERSION));
    }
    if version_code < proto::COMPANION_VERSION_CODE {
        return Err("companion app is still outdated after reinstall".into());
    }
    // Android switches the accessibility service off whenever the app is updated or reinstalled, and
    // the user may have turned it off: without it every click and key is silently ignored. This used
    // to run only on a fresh install, so check it on every connect.
    let input_note = match adb.enable_accessibility(serial) {
        Ok(()) => String::new(),
        Err(e) => format!("could not enable input control: {e}"),
    };
    *c.sock.lock().unwrap() = Some(stream.try_clone().map_err(|e| e.to_string())?);
    c.shared.update(|s| {
        s.device_name = format!("{name} (Android SDK {sdk})");
        s.phase = Phase::Streaming;
        s.note = input_note;
    });

    let result = stream_loop(c, stream);
    adb.remove_forward(serial, port);
    result
}

enum DecJob {
    Config(Vec<u8>),
    Frame(Vec<u8>),
}

fn stream_loop(c: &ConnCtx, mut stream: TcpStream) -> Result<(), String> {
    let shared = c.shared.clone();

    // Writer thread.
    let (in_tx, in_rx) = channel::<HostMsg>();
    *shared.input_tx.lock().unwrap() = Some(in_tx.clone());
    let mut wstream = stream.try_clone().map_err(|e| e.to_string())?;
    let writer = std::thread::Builder::new()
        .name("writer".into())
        .spawn(move || {
            while let Ok(m) = in_rx.recv() {
                let (ty, payload) = m.encode();
                if proto::write_frame(&mut wstream, ty, 0, 0, &payload).is_err() {
                    break;
                }
            }
        })
        .unwrap();

    // Decoder thread.
    let (dec_tx, dec_rx) = sync_channel::<DecJob>(24);
    let dshared = shared.clone();
    let decoder = std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || decode_loop(dec_rx, dshared))
        .unwrap();

    let _ = in_tx.send(HostMsg::RequestKeyframe);
    let mut audio: Option<AudioPlayer> = None;
    let mut cfg: Vec<u8> = vec![];
    let mut vsize = (0u32, 0u32);
    let mut audio_cfg: Option<(u32, u8)> = None;
    let mut skip_until_key = false;

    let result = loop {
        let frame = match proto::read_frame(&mut stream) {
            Ok(f) => f,
            Err(e) => break Err(if c.stopped() { String::new() } else { format!("connection lost: {e}") }),
        };
        let Some(msg) = DeviceMsg::parse(frame) else { continue };

        if c.rec_init.swap(false, Ordering::AcqRel) {
            if let Some(r) = c.rec.lock().unwrap().as_ref() {
                r.video_config(vsize.0, vsize.1, &cfg);
                if let Some((rate, ch)) = audio_cfg {
                    r.audio_config(rate, ch);
                }
            }
        }

        match msg {
            DeviceMsg::VideoConfig { width, height, rotation, codec_config } => {
                cfg = codec_config.clone();
                vsize = (width, height);
                shared.update(|s| s.note = format!("video {width}x{height} rot {}°", rotation as u32 * 90));
                if let Some(r) = c.rec.lock().unwrap().as_ref() {
                    r.video_config(width, height, &cfg);
                }
                let _ = dec_tx.send(DecJob::Config(codec_config));
                skip_until_key = false;
            }
            DeviceMsg::VideoFrame { key, data, pts_us } => {
                if let Some(r) = c.rec.lock().unwrap().as_ref() {
                    r.video(key, &data, pts_us);
                }
                if skip_until_key && !key {
                    continue;
                }
                match dec_tx.try_send(DecJob::Frame(data)) {
                    Ok(()) => skip_until_key = false,
                    Err(TrySendError::Full(_)) => {
                        // Decoder is behind: drop until the next keyframe to bound latency.
                        skip_until_key = true;
                        let _ = in_tx.send(HostMsg::RequestKeyframe);
                    }
                    Err(TrySendError::Disconnected(_)) => break Err("decoder stopped".into()),
                }
            }
            DeviceMsg::AudioConfig { sample_rate, channels } => {
                audio_cfg = Some((sample_rate, channels));
                if let Some(r) = c.rec.lock().unwrap().as_ref() {
                    r.audio_config(sample_rate, channels);
                }
                audio = match AudioPlayer::new(sample_rate, channels) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        shared.update(|s| s.note = format!("audio output unavailable: {e}"));
                        None
                    }
                };
            }
            DeviceMsg::AudioData { pcm, .. } => {
                if let Some(r) = c.rec.lock().unwrap().as_ref() {
                    r.audio(&pcm);
                }
                if let Some(a) = &audio {
                    if !shared.muted.load(Ordering::Relaxed) {
                        a.push(&pcm);
                    }
                }
            }
            DeviceMsg::Status { audio, input_ready } => shared.update(|s| {
                s.audio = audio;
                s.input_ready = input_ready;
            }),
            DeviceMsg::Hello { .. } | DeviceMsg::Unknown(_) => {}
        }
    };

    drop(dec_tx);
    *shared.input_tx.lock().unwrap() = None;
    drop(in_tx);
    let _ = decoder.join();
    let _ = writer.join();
    match result {
        Err(e) if e.is_empty() => Ok(()),
        other => other,
    }
}

fn decode_loop(rx: Receiver<DecJob>, shared: Arc<Shared>) {
    let mut dec = match H264::new() {
        Ok(d) => d,
        Err(e) => {
            shared.update(|s| s.phase = Phase::Error(format!("decoder init failed: {e}")));
            return;
        }
    };
    let seq = AtomicU64::new(0);
    let mut window_start = Instant::now();
    let mut frames = 0u32;
    while let Ok(job) = rx.recv() {
        let rot = shared.view_rotation.load(Ordering::Relaxed);
        let out = match job {
            DecJob::Config(c) => {
                // Resolution may have changed: start from a clean decoder.
                if let Ok(d) = H264::new() {
                    dec = d;
                }
                dec.decode(&c, rot)
            }
            DecJob::Frame(d) => dec.decode(&d, rot),
        };
        if let Some(p) = out {
            let size = (p.width, p.height);
            *shared.frame.lock().unwrap() = Some(VideoFrame { width: p.width, height: p.height, bgra: p.bgra });
            seq.fetch_add(1, Ordering::Relaxed);
            frames += 1;
            let elapsed = window_start.elapsed();
            let fps = if elapsed >= Duration::from_secs(1) {
                let f = (frames as f32 / elapsed.as_secs_f32()).round() as u32;
                frames = 0;
                window_start = Instant::now();
                Some(f)
            } else {
                None
            };
            shared.update(|s| {
                s.frame_size = size;
                if let Some(f) = fps {
                    s.fps = f;
                }
            });
        }
    }
}
