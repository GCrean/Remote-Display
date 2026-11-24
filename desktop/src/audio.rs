//! PCM16 playback through the default output device using a small jitter buffer.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, StreamConfig};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub struct AudioPlayer {
    buf: Arc<Mutex<VecDeque<i16>>>,
    max_samples: usize,
    _stream: cpal::Stream,
}

impl AudioPlayer {
    /// `sample_rate`/`channels` describe the incoming PCM; the device is opened with the same
    /// parameters (shared-mode WASAPI/CoreAudio/ALSA resample for us).
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no audio output device")?;
        let config = StreamConfig { channels: channels as _, sample_rate: sample_rate as _, buffer_size: BufferSize::Default };
        let buf: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let cb_buf = buf.clone();
        let stream = device
            .build_output_stream(
                config,
                move |out: &mut [i16], _| {
                    let mut q = cb_buf.lock().unwrap();
                    for s in out.iter_mut() {
                        *s = q.pop_front().unwrap_or(0);
                    }
                },
                |e| eprintln!("audio stream error: {e}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        // Cap latency at ~300 ms; older audio is dropped so audio never drifts behind video.
        let max_samples = (sample_rate as usize * channels as usize) * 3 / 10;
        Ok(Self { buf, max_samples, _stream: stream })
    }

    pub fn push(&self, pcm: &[u8]) {
        let mut q = self.buf.lock().unwrap();
        q.extend(pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        let len = q.len();
        if len > self.max_samples {
            q.drain(..len - self.max_samples);
        }
    }
}
