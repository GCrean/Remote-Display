//! Session recording to a single `.mp4` (H.264 + AAC-LC) on a dedicated thread, so the
//! UI/decoder never block on disk I/O or audio encoding.
//!
//! The recording has one constant video size (the "canvas") because most players ignore later
//! sample entries. The canvas is chosen only once the device's orientation has settled (about 1.5 s of
//! stable size, at most 5 s), so an app that rotates right after launch does not leave the whole
//! recording squeezed into the pre-rotation orientation; video and audio that arrive meanwhile are
//! buffered and replayed with their original timing. Frames at the canvas size are muxed as received
//! (lossless); frames of another size (a later rotation, or the brief pre-rotation start) are decoded,
//! letterboxed into the canvas and re-encoded (see `transcode`). The device's PCM is AAC-encoded here.
//! The audio track is aligned to the video track using host arrival times (silence inserted, or
//! leading samples dropped, so both start together). The file is only playable after `stop()` writes
//! the index, so quit via Stop/Disconnect rather than killing the app.

use fdk_aac_rs::encoder::{Aot, Bitrate, ChannelMode, Encoder, Transport};
use crate::transcode::Transcoder;
use rd_mp4mux::{annexb_to_avcc, annexb_to_avcc_in_band, parameter_sets, Mp4Writer};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

enum Job {
    VideoConfig { width: u32, height: u32, annexb: Vec<u8> },
    Video { key: bool, data: Vec<u8>, pts_us: u64, at: Instant },
    AudioConfig { rate: u32, channels: u8 },
    Audio { pcm: Vec<u8>, at: Instant },
    Stop,
}

pub struct Recorder {
    pub path: PathBuf,
    tx: Sender<Job>,
    thread: Option<JoinHandle<String>>,
}

impl Recorder {
    pub fn start(dir: &Path, stem: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut path = dir.join(format!("{stem}.mp4"));
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!("{stem}-{n}.mp4"));
            n += 1;
        }
        let mux = Mp4Writer::new(BufWriter::new(File::create(&path)?))?;
        let (tx, rx) = channel::<Job>();
        let out_path = path.clone();
        let thread = std::thread::Builder::new().name("recorder".into()).spawn(move || run(rx, mux, path))?;
        Ok(Self { path: out_path, tx, thread: Some(thread) })
    }
    pub fn video_config(&self, width: u32, height: u32, annexb: &[u8]) {
        let _ = self.tx.send(Job::VideoConfig { width, height, annexb: annexb.to_vec() });
    }
    pub fn video(&self, key: bool, data: &[u8], pts_us: u64) {
        let _ = self.tx.send(Job::Video { key, data: data.to_vec(), pts_us, at: Instant::now() });
    }
    pub fn audio_config(&self, rate: u32, channels: u8) {
        let _ = self.tx.send(Job::AudioConfig { rate, channels });
    }
    pub fn audio(&self, pcm: &[u8]) {
        let _ = self.tx.send(Job::Audio { pcm: pcm.to_vec(), at: Instant::now() });
    }
    /// Finish the file; returns a human-readable summary.
    pub fn stop(mut self) -> String {
        let _ = self.tx.send(Job::Stop);
        self.thread.take().map(|t| t.join().unwrap_or_else(|_| "recorder crashed".into())).unwrap_or_default()
    }
}

struct Audio {
    enc: Encoder,
    out: Vec<u8>,
    rate: u32,
    channels: usize,
    pcm: Vec<i16>,
    started: bool,
}

impl Audio {
    fn new(rate: u32, channels: u8) -> Result<Self, String> {
        let mode = match channels {
            1 => ChannelMode::Mono,
            2 => ChannelMode::Stereo,
            n => return Err(format!("{n} channels unsupported")),
        };
        let enc = Encoder::builder()
            .aot(Aot::AacLc)
            .sample_rate(rate)
            .channel_mode(mode)
            .bitrate(Bitrate::Cbr(128_000))
            .transport(Transport::Raw)
            .build()
            .map_err(|e| format!("{e:?}"))?;
        Ok(Self { enc, out: vec![0; 8192], rate, channels: channels as usize, pcm: vec![], started: false })
    }

    fn emit(&mut self, mux: &mut Mp4Writer<BufWriter<File>>, input: &[i16]) -> bool {
        match self.enc.encode(input, &mut self.out) {
            Ok(r) if r.out_bytes() > 0 => {
                let _ = mux.add_audio_frame(&self.out[..r.out_bytes()]);
                true
            }
            _ => false,
        }
    }

    fn drain(&mut self, mux: &mut Mp4Writer<BufWriter<File>>, flush: bool) {
        let hop = 1024 * self.channels;
        if flush && !self.pcm.is_empty() && self.pcm.len() % hop != 0 {
            self.pcm.resize(self.pcm.len().div_ceil(hop) * hop, 0);
        }
        while self.pcm.len() >= hop {
            let frame: Vec<i16> = self.pcm.drain(..hop).collect();
            self.emit(mux, &frame);
        }
        if flush {
            // Drain the encoder's internal delay (bounded: a couple of frames).
            for _ in 0..4 {
                match self.enc.flush(&mut self.out) {
                    Ok(r) if r.out_bytes() > 0 => {
                        let _ = mux.add_audio_frame(&self.out[..r.out_bytes()]);
                    }
                    _ => break,
                }
            }
        }
    }
}

/// How long the video size must stay unchanged before the canvas is chosen, and the longest we wait.
const SETTLE: Duration = Duration::from_millis(1500);
const MAX_SETTLE: Duration = Duration::from_millis(5000);

struct Rec {
    mux: Mp4Writer<BufWriter<File>>,
    last_size: (u32, u32),
    sizes: HashMap<Vec<u8>, (u32, u32)>, // SPS -> (w, h)
    canvas: Option<(u32, u32)>,
    transcoder: Option<Transcoder>,
    transcoded_frames: u64,
    base_pts: Option<u64>,
    video_start: Option<Instant>,
    audio: Option<Audio>,
    audio_cfg_seen: bool,
    note: Option<String>,
    // canvas selection
    committed: bool,
    pending: Vec<Job>,
    seen_size: Option<(u32, u32)>,
    stable_since: Option<Instant>,
    first_key_at: Option<Instant>,
}

impl Rec {
    fn new(mux: Mp4Writer<BufWriter<File>>) -> Self {
        Rec {
            mux,
            last_size: (0, 0),
            sizes: HashMap::new(),
            canvas: None,
            transcoder: None,
            transcoded_frames: 0,
            base_pts: None,
            video_start: None,
            audio: None,
            audio_cfg_seen: false,
            note: None,
            committed: false,
            pending: vec![],
            seen_size: None,
            stable_since: None,
            first_key_at: None,
        }
    }

    /// Size of the stream a keyframe belongs to. The SPS identifies the encoder instance, so late
    /// frames of the previous encoder after a rotation cannot confuse us.
    fn keyframe_size(&self, data: &[u8]) -> Option<(u32, u32)> {
        let (sps, _) = parameter_sets(data)?;
        let s = self.sizes.get(&sps).copied().unwrap_or(self.last_size);
        (s.0 > 0).then_some(s)
    }

    fn on_job(&mut self, job: Job) {
        match job {
            Job::VideoConfig { width, height, annexb } => {
                self.last_size = (width, height);
                if let Some((sps, _)) = parameter_sets(&annexb) {
                    self.sizes.insert(sps, (width, height));
                }
            }
            Job::AudioConfig { rate, channels } => {
                if !self.audio_cfg_seen {
                    self.audio_cfg_seen = true;
                    match Audio::new(rate, channels) {
                        Ok(a) => {
                            self.mux.set_audio_config(rate, channels);
                            self.audio = Some(a);
                        }
                        Err(e) => self.note = Some(format!("AAC encoder unavailable ({e}); recording video only")),
                    }
                }
            }
            Job::Video { .. } | Job::Audio { .. } => {
                if self.committed {
                    self.process(job);
                } else {
                    self.defer(job);
                }
            }
            Job::Stop => {}
        }
    }

    /// Before the canvas is chosen: hold media back, and watch whether the size is still changing.
    fn defer(&mut self, job: Job) {
        enum Act {
            Drop,
            Keep,
            Video { key: bool, at: Instant, size: Option<(u32, u32)> },
        }
        let act = match &job {
            Job::Video { key, data, at, .. } => {
                if self.first_key_at.is_none() && !*key {
                    Act::Drop // must start on a keyframe
                } else {
                    Act::Video { key: *key, at: *at, size: if *key { self.keyframe_size(data) } else { None } }
                }
            }
            Job::Audio { .. } => {
                if self.first_key_at.is_some() {
                    Act::Keep
                } else {
                    Act::Drop
                }
            }
            _ => Act::Drop,
        };
        match act {
            Act::Drop => {}
            Act::Keep => self.pending.push(job),
            Act::Video { key, at, size } => {
                if key {
                    match size {
                        Some(sz) => {
                            if self.seen_size != Some(sz) {
                                self.seen_size = Some(sz);
                                self.stable_since = Some(at);
                            }
                        }
                        None if self.first_key_at.is_none() => return, // size unknown yet
                        None => {}
                    }
                }
                let first = *self.first_key_at.get_or_insert(at);
                self.pending.push(job);
                let stable = self.stable_since.map_or(Duration::ZERO, |s| at.saturating_duration_since(s));
                if stable >= SETTLE || at.saturating_duration_since(first) >= MAX_SETTLE {
                    self.commit();
                }
            }
        }
    }

    /// Choose the canvas (the settled size) and replay everything held back, in order.
    fn commit(&mut self) {
        if self.committed {
            return;
        }
        self.committed = true;
        if self.canvas.is_none() {
            self.canvas = self.seen_size;
        }
        for job in std::mem::take(&mut self.pending) {
            self.process(job);
        }
    }

    fn process(&mut self, job: Job) {
        match job {
            Job::Video { key, data, pts_us, at } => {
                if self.base_pts.is_none() && !key {
                    return;
                }
                if key {
                    let Some((sps, pps)) = parameter_sets(&data) else { return };
                    let size = self.sizes.get(&sps).copied().unwrap_or(self.last_size);
                    if size.0 == 0 {
                        return;
                    }
                    let canvas = *self.canvas.get_or_insert(size);
                    if self.base_pts.is_none() {
                        self.base_pts = Some(pts_us);
                        self.video_start = Some(at);
                    }
                    if size == canvas {
                        // Canvas size: lossless stream copy.
                        self.mux.set_video_config(size.0, size.1, sps, pps);
                        self.transcoder = None;
                    } else if self.transcoder.is_none() {
                        match Transcoder::new(canvas.0, canvas.1, 8_000_000) {
                            Ok(t) => self.transcoder = Some(t),
                            Err(e) => {
                                self.note = Some(format!("rotation transcoder unavailable: {e}"));
                                return;
                            }
                        }
                    }
                }
                let t = pts_us.saturating_sub(self.base_pts.unwrap());
                match self.transcoder.as_mut() {
                    None => {
                        let sample = if key { annexb_to_avcc_in_band(&data) } else { annexb_to_avcc(&data) };
                        let _ = self.mux.add_video_sample(&sample, key, t);
                    }
                    Some(tc) => {
                        if let Some((out, k)) = tc.push(&data, t / 1000) {
                            if k {
                                if let (Some((sps, pps)), Some(c)) = (parameter_sets(&out), self.canvas) {
                                    self.mux.set_video_config(c.0, c.1, sps, pps);
                                }
                            }
                            self.transcoded_frames += 1;
                            let sample = if k { annexb_to_avcc_in_band(&out) } else { annexb_to_avcc(&out) };
                            let _ = self.mux.add_video_sample(&sample, k, t);
                        }
                    }
                }
            }
            Job::Audio { pcm, at } => {
                let Some(vs) = self.video_start else { return };
                let Some(a) = self.audio.as_mut() else { return };
                let mut samples: Vec<i16> = pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                if !a.started {
                    a.started = true;
                    // Start of this chunk on the host clock, relative to the first video frame.
                    let chunk_secs = samples.len() as f64 / (a.rate as f64 * a.channels as f64);
                    let offset = at.saturating_duration_since(vs).as_secs_f64() - chunk_secs;
                    let n = (offset.abs() * a.rate as f64) as usize * a.channels;
                    if offset > 0.0 {
                        a.pcm.resize(n, 0); // silence so audio and video start together
                    } else {
                        samples.drain(..n.min(samples.len()));
                    }
                }
                a.pcm.extend(samples);
                a.drain(&mut self.mux, false);
            }
            _ => {}
        }
    }
}

fn run(rx: Receiver<Job>, mux: Mp4Writer<BufWriter<File>>, path: PathBuf) -> String {
    let mut rec = Rec::new(mux);
    while let Ok(job) = rx.recv() {
        if matches!(job, Job::Stop) {
            break;
        }
        rec.on_job(job);
    }
    // Stopped before the orientation settled: use what we have.
    if !rec.committed && !rec.pending.is_empty() {
        rec.commit();
    }

    let Rec { mut mux, audio, transcoded_frames, note, .. } = rec;
    if let Some(mut a) = audio {
        if a.started {
            a.drain(&mut mux, true);
        }
    }
    let frames = mux.video_samples();
    let finished = mux.finish();
    if frames == 0 {
        let _ = std::fs::remove_file(&path);
        return "recording discarded: no video frames were received".into();
    }
    match finished {
        Ok(_) => {
            let mut text = note.map(|e| format!(" ({e})")).unwrap_or_default();
            if transcoded_frames > 0 {
                text.push_str(&format!(" ({transcoded_frames} frames letterboxed after rotation)"));
            }
            format!("saved {}{text}", path.display())
        }
        Err(e) => format!("failed to finalise {}: {e}", path.display()),
    }
}
