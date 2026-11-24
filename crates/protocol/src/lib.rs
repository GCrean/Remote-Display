//! Wire protocol shared by the desktop host and the Android companion.
//!
//! One ordered byte stream (ADB-forwarded socket) carries every message as a
//! fixed 14-byte header followed by `len` payload bytes. All integers are
//! little-endian.
//!
//! ```text
//! | type u8 | flags u8 | len u32 | pts_us u64 | payload[len] |
//! ```

pub mod coords;

use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u16 = 1;
/// Abstract unix socket name the companion listens on (`localabstract:`).
pub const ABSTRACT_SOCKET: &str = "remote_display";
pub const PACKAGE: &str = "com.thunderlight.remote.display";
/// Earlier builds of the companion used these package names; the desktop removes them.
pub const LEGACY_PACKAGES: &[&str] = &["dev.remotedisplay.companion"];
pub const MAIN_ACTIVITY: &str = "com.thunderlight.remote.display/.MainActivity";
pub const ACCESSIBILITY_SERVICE: &str = "com.thunderlight.remote.display/com.thunderlight.remote.display.InputService";
/// Must match `versionCode` in android/app/build.gradle.kts.
pub const COMPANION_VERSION_CODE: u32 = 10;

pub const HEADER_LEN: usize = 14;
pub const MAX_PAYLOAD: u32 = 32 * 1024 * 1024;

pub const FLAG_KEYFRAME: u8 = 1;

// Device -> host
pub const T_HELLO: u8 = 0x01;
pub const T_VIDEO_CONFIG: u8 = 0x02;
pub const T_VIDEO_FRAME: u8 = 0x03;
pub const T_AUDIO_CONFIG: u8 = 0x04;
pub const T_AUDIO_DATA: u8 = 0x05;
pub const T_STATUS: u8 = 0x06;
// Host -> device
pub const T_TOUCH: u8 = 0x10;
pub const T_KEY: u8 = 0x11;
pub const T_SCROLL: u8 = 0x12;
pub const T_TEXT: u8 = 0x13;
pub const T_REQUEST_KEYFRAME: u8 = 0x14;
pub const T_NAV: u8 = 0x15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub ty: u8,
    pub flags: u8,
    pub pts_us: u64,
    pub payload: Vec<u8>,
}

pub fn write_frame<W: Write>(w: &mut W, ty: u8, flags: u8, pts_us: u64, payload: &[u8]) -> io::Result<()> {
    let mut h = [0u8; HEADER_LEN];
    h[0] = ty;
    h[1] = flags;
    h[2..6].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    h[6..14].copy_from_slice(&pts_us.to_le_bytes());
    w.write_all(&h)?;
    w.write_all(payload)
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<RawFrame> {
    let mut h = [0u8; HEADER_LEN];
    r.read_exact(&mut h)?;
    let len = u32::from_le_bytes(h[2..6].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame too large: {len}")));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(RawFrame { ty: h[0], flags: h[1], pts_us: u64::from_le_bytes(h[6..14].try_into().unwrap()), payload })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioStatus {
    /// Not reported yet.
    Unknown,
    Streaming,
    /// Android < 10.
    Unsupported,
    /// RECORD_AUDIO not granted.
    PermissionDenied,
    /// Capture could not start (e.g. device policy).
    Failed,
}

impl AudioStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Streaming,
            2 => Self::Unsupported,
            3 => Self::PermissionDenied,
            4 => Self::Failed,
            _ => Self::Unknown,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "audio: pending",
            Self::Streaming => "audio: streaming",
            Self::Unsupported => "audio: needs Android 10+",
            Self::PermissionDenied => "audio: permission denied",
            Self::Failed => "audio: capture failed",
        }
    }
}

/// Messages sent by the Android device.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceMsg {
    Hello { version: u16, version_code: u32, sdk: u32, name: String },
    /// Sent at start and after every orientation change / encoder restart.
    /// `rotation` is the display rotation in 90° steps (Surface.ROTATION_*).
    /// `codec_config` is Annex-B SPS+PPS.
    VideoConfig { width: u32, height: u32, rotation: u8, codec_config: Vec<u8> },
    VideoFrame { pts_us: u64, key: bool, data: Vec<u8> },
    AudioConfig { sample_rate: u32, channels: u8 },
    AudioData { pts_us: u64, pcm: Vec<u8> },
    Status { audio: AudioStatus, input_ready: bool },
    Unknown(u8),
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn i32_at(b: &[u8], o: usize) -> Option<i32> {
    u32_at(b, o).map(|v| v as i32)
}

impl DeviceMsg {
    pub fn parse(f: RawFrame) -> Option<DeviceMsg> {
        let p = &f.payload;
        Some(match f.ty {
            T_HELLO => DeviceMsg::Hello {
                version: u16::from_le_bytes(p.get(0..2)?.try_into().ok()?),
                version_code: u32_at(p, 2)?,
                sdk: u32_at(p, 6)?,
                name: String::from_utf8_lossy(p.get(10..)?).into_owned(),
            },
            T_VIDEO_CONFIG => DeviceMsg::VideoConfig {
                width: u32_at(p, 0)?,
                height: u32_at(p, 4)?,
                rotation: *p.get(8)?,
                codec_config: p.get(9..)?.to_vec(),
            },
            T_VIDEO_FRAME => DeviceMsg::VideoFrame { pts_us: f.pts_us, key: f.flags & FLAG_KEYFRAME != 0, data: f.payload },
            T_AUDIO_CONFIG => DeviceMsg::AudioConfig { sample_rate: u32_at(p, 0)?, channels: *p.get(4)? },
            T_AUDIO_DATA => DeviceMsg::AudioData { pts_us: f.pts_us, pcm: f.payload },
            T_STATUS => DeviceMsg::Status { audio: AudioStatus::from_u8(*p.first()?), input_ready: *p.get(1)? != 0 },
            other => DeviceMsg::Unknown(other),
        })
    }

    /// Serialise (used by tests and by a possible mock device).
    pub fn encode(&self) -> (u8, u8, u64, Vec<u8>) {
        match self {
            DeviceMsg::Hello { version, version_code, sdk, name } => {
                let mut p = version.to_le_bytes().to_vec();
                p.extend(version_code.to_le_bytes());
                p.extend(sdk.to_le_bytes());
                p.extend(name.as_bytes());
                (T_HELLO, 0, 0, p)
            }
            DeviceMsg::VideoConfig { width, height, rotation, codec_config } => {
                let mut p = width.to_le_bytes().to_vec();
                p.extend(height.to_le_bytes());
                p.push(*rotation);
                p.extend(codec_config);
                (T_VIDEO_CONFIG, 0, 0, p)
            }
            DeviceMsg::VideoFrame { pts_us, key, data } => (T_VIDEO_FRAME, if *key { FLAG_KEYFRAME } else { 0 }, *pts_us, data.clone()),
            DeviceMsg::AudioConfig { sample_rate, channels } => {
                let mut p = sample_rate.to_le_bytes().to_vec();
                p.push(*channels);
                (T_AUDIO_CONFIG, 0, 0, p)
            }
            DeviceMsg::AudioData { pts_us, pcm } => (T_AUDIO_DATA, 0, *pts_us, pcm.clone()),
            DeviceMsg::Status { audio, input_ready } => {
                let a = match audio {
                    AudioStatus::Unknown => 0,
                    AudioStatus::Streaming => 1,
                    AudioStatus::Unsupported => 2,
                    AudioStatus::PermissionDenied => 3,
                    AudioStatus::Failed => 4,
                };
                (T_STATUS, 0, 0, vec![a, *input_ready as u8])
            }
            DeviceMsg::Unknown(t) => (*t, 0, 0, vec![]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchAction {
    Down = 0,
    Move = 1,
    Up = 2,
    Cancel = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAction {
    Back = 1,
    Home = 2,
    Recents = 3,
    Notifications = 4,
}

/// Messages sent by the desktop host.
#[derive(Debug, Clone, PartialEq)]
pub enum HostMsg {
    /// Coordinates are pixels in the *video frame* of size `frame_w x frame_h`;
    /// the device rescales to its real display so rotation races are harmless.
    Touch { action: TouchAction, pointer: u8, x: u32, y: u32, frame_w: u32, frame_h: u32 },
    Key { down: bool, keycode: i32, meta: i32 },
    Scroll { x: u32, y: u32, frame_w: u32, frame_h: u32, dx: i32, dy: i32 },
    Text(String),
    RequestKeyframe,
    Nav(NavAction),
}

impl HostMsg {
    pub fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            HostMsg::Touch { action, pointer, x, y, frame_w, frame_h } => {
                let mut p = vec![*action as u8, *pointer];
                for v in [x, y, frame_w, frame_h] {
                    p.extend(v.to_le_bytes());
                }
                (T_TOUCH, p)
            }
            HostMsg::Key { down, keycode, meta } => {
                let mut p = vec![*down as u8];
                p.extend(keycode.to_le_bytes());
                p.extend(meta.to_le_bytes());
                (T_KEY, p)
            }
            HostMsg::Scroll { x, y, frame_w, frame_h, dx, dy } => {
                let mut p = Vec::new();
                for v in [x, y, frame_w, frame_h] {
                    p.extend(v.to_le_bytes());
                }
                p.extend(dx.to_le_bytes());
                p.extend(dy.to_le_bytes());
                (T_SCROLL, p)
            }
            HostMsg::Text(s) => (T_TEXT, s.as_bytes().to_vec()),
            HostMsg::RequestKeyframe => (T_REQUEST_KEYFRAME, vec![]),
            HostMsg::Nav(n) => (T_NAV, vec![*n as u8]),
        }
    }

    pub fn parse(f: &RawFrame) -> Option<HostMsg> {
        let p = &f.payload;
        Some(match f.ty {
            T_TOUCH => HostMsg::Touch {
                action: match *p.first()? {
                    0 => TouchAction::Down,
                    1 => TouchAction::Move,
                    2 => TouchAction::Up,
                    _ => TouchAction::Cancel,
                },
                pointer: *p.get(1)?,
                x: u32_at(p, 2)?,
                y: u32_at(p, 6)?,
                frame_w: u32_at(p, 10)?,
                frame_h: u32_at(p, 14)?,
            },
            T_KEY => HostMsg::Key { down: *p.first()? != 0, keycode: i32_at(p, 1)?, meta: i32_at(p, 5)? },
            T_SCROLL => HostMsg::Scroll {
                x: u32_at(p, 0)?,
                y: u32_at(p, 4)?,
                frame_w: u32_at(p, 8)?,
                frame_h: u32_at(p, 12)?,
                dx: i32_at(p, 16)?,
                dy: i32_at(p, 20)?,
            },
            T_TEXT => HostMsg::Text(String::from_utf8_lossy(p).into_owned()),
            T_REQUEST_KEYFRAME => HostMsg::RequestKeyframe,
            T_NAV => HostMsg::Nav(match *p.first()? {
                1 => NavAction::Back,
                2 => NavAction::Home,
                3 => NavAction::Recents,
                _ => NavAction::Notifications,
            }),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, T_VIDEO_FRAME, FLAG_KEYFRAME, 42, &[1, 2, 3]).unwrap();
        let f = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(DeviceMsg::parse(f), Some(DeviceMsg::VideoFrame { pts_us: 42, key: true, data: vec![1, 2, 3] }));
    }

    #[test]
    fn host_roundtrip() {
        let m = HostMsg::Touch { action: TouchAction::Move, pointer: 0, x: 10, y: 20, frame_w: 1080, frame_h: 1920 };
        let (ty, payload) = m.encode();
        let f = RawFrame { ty, flags: 0, pts_us: 0, payload };
        assert_eq!(HostMsg::parse(&f), Some(m));
    }

    #[test]
    fn rejects_oversize() {
        let mut h = [0u8; HEADER_LEN];
        h[2..6].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_frame(&mut h.as_slice()).is_err());
    }
}
