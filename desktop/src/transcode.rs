//! Fixed-canvas transcoding used by the recorder when the device rotates mid-recording.
//!
//! An MP4 track must have one size for players to behave (most ignore later sample entries), so
//! frames whose size differs from the recording canvas are decoded, letterboxed into the canvas
//! and re-encoded. Recording stays a lossless stream copy for as long as the size is unchanged.

use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, UsageType};
use openh264::formats::{YUVSlices, YUVSource};
use openh264::Timestamp;

pub struct Transcoder {
    dec: Decoder,
    enc: Encoder,
    cw: usize,
    ch: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    /// Source size the bars were last cleared for.
    cleared_for: (usize, usize),
}

impl Transcoder {
    pub fn new(canvas_w: u32, canvas_h: u32, bitrate_bps: u32) -> Result<Self, String> {
        let (cw, ch) = (canvas_w as usize & !1, canvas_h as usize & !1);
        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(30.0))
            .usage_type(UsageType::ScreenContentRealTime)
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(60));
        let api = openh264::OpenH264API::from_source();
        Ok(Self {
            dec: Decoder::new().map_err(|e| e.to_string())?,
            enc: Encoder::with_api_config(api, cfg).map_err(|e| e.to_string())?,
            cw,
            ch,
            y: vec![16; cw * ch],
            u: vec![128; cw * ch / 4],
            v: vec![128; cw * ch / 4],
            cleared_for: (0, 0),
        })
    }

    /// Decode one Annex-B access unit and return the re-encoded canvas frame (Annex-B, keyframe).
    pub fn push(&mut self, au: &[u8], pts_ms: u64) -> Option<(Vec<u8>, bool)> {
        let yuv = self.dec.decode(au).ok()??;
        let (sw, sh) = yuv.dimensions();
        let (ys, us, vs) = yuv.strides();
        if self.cleared_for != (sw, sh) {
            self.y.fill(16);
            self.u.fill(128);
            self.v.fill(128);
            self.cleared_for = (sw, sh);
        }
        let (cw, ch) = (self.cw, self.ch);
        // Fit (never upscale beyond the canvas), even-aligned, centred.
        let scale = (cw as f32 / sw as f32).min(ch as f32 / sh as f32);
        let (rw, rh) = (((sw as f32 * scale) as usize & !1).max(2), ((sh as f32 * scale) as usize & !1).max(2));
        let (ox, oy) = (((cw - rw) / 2) & !1, ((ch - rh) / 2) & !1);
        blit_scaled(yuv.y(), ys, sw, sh, &mut self.y, cw, ox, oy, rw, rh);
        blit_scaled(yuv.u(), us, sw / 2, sh / 2, &mut self.u, cw / 2, ox / 2, oy / 2, rw / 2, rh / 2);
        blit_scaled(yuv.v(), vs, sw / 2, sh / 2, &mut self.v, cw / 2, ox / 2, oy / 2, rw / 2, rh / 2);

        let slices = YUVSlices::new((&self.y, &self.u, &self.v), (cw, ch), (cw, cw / 2, cw / 2));
        let bs = self.enc.encode_at(&slices, Timestamp::from_millis(pts_ms)).ok()?;
        let key = match bs.frame_type() {
            FrameType::IDR | FrameType::I => true,
            FrameType::P => false,
            _ => return None,
        };
        let mut out = Vec::new();
        bs.write_vec(&mut out);
        (!out.is_empty()).then_some((out, key))
    }
}

/// Copy `src` (sw x sh, `sstride`) into `dst` (`dstride`) at (ox,oy) scaled to rw x rh using
/// area averaging (exact copy when the sizes match).
#[allow(clippy::too_many_arguments)]
fn blit_scaled(src: &[u8], sstride: usize, sw: usize, sh: usize, dst: &mut [u8], dstride: usize, ox: usize, oy: usize, rw: usize, rh: usize) {
    if sw == rw && sh == rh {
        for y in 0..rh {
            dst[(oy + y) * dstride + ox..(oy + y) * dstride + ox + rw].copy_from_slice(&src[y * sstride..y * sstride + sw]);
        }
        return;
    }
    let xr: Vec<(usize, usize)> = (0..rw).map(|x| (x * sw / rw, ((x + 1) * sw / rw).max(x * sw / rw + 1).min(sw))).collect();
    for y in 0..rh {
        let (y0, y1) = (y * sh / rh, ((y + 1) * sh / rh).max(y * sh / rh + 1).min(sh));
        let drow = &mut dst[(oy + y) * dstride + ox..(oy + y) * dstride + ox + rw];
        for (x, d) in drow.iter_mut().enumerate() {
            let (x0, x1) = xr[x];
            let mut sum = 0u32;
            for sy in y0..y1 {
                for &p in &src[sy * sstride + x0..sy * sstride + x1] {
                    sum += p as u32;
                }
            }
            *d = (sum / ((y1 - y0) * (x1 - x0)) as u32) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_keeps_flat_planes_flat_and_copy_is_exact() {
        let src = vec![200u8; 100 * 50];
        let mut dst = vec![0u8; 40 * 40];
        blit_scaled(&src, 100, 100, 50, &mut dst, 40, 0, 10, 40, 20);
        assert!(dst[10 * 40..30 * 40].iter().all(|&p| p == 200));
        assert!(dst[..10 * 40].iter().all(|&p| p == 0), "bars untouched");

        let ramp: Vec<u8> = (0..16).collect();
        let mut out = vec![0u8; 16];
        blit_scaled(&ramp, 4, 4, 4, &mut out, 4, 0, 0, 4, 4);
        assert_eq!(out, ramp);
    }

    /// Encode a landscape frame, push it through a portrait canvas: output must be portrait-sized
    /// with black bars above and below, and must decode.
    #[test]
    fn landscape_frame_is_letterboxed_into_portrait_canvas() {
        let (w, h) = (128usize, 64usize);
        let (y, u, v) = (vec![180u8; w * h], vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let mut enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new()).unwrap();
        let src = YUVSlices::new((&y, &u, &v), (w, h), (w, w / 2, w / 2));
        let mut au = Vec::new();
        enc.encode(&src).unwrap().write_vec(&mut au);

        let mut tc = Transcoder::new(64, 128, 1_000_000).unwrap();
        let (out, key) = tc.push(&au, 0).expect("transcoded frame");
        assert!(key, "first transcoded frame must be a keyframe");

        let mut dec = Decoder::new().unwrap();
        let pic = dec.decode(&out).unwrap().expect("decodes");
        assert_eq!(pic.dimensions(), (64, 128));
        let (ys, _, _) = pic.strides();
        let row = |r: usize| pic.y()[r * ys + 32];
        assert!(row(2) < 40, "top bar should be black, got {}", row(2));
        assert!(row(125) < 40, "bottom bar should be black, got {}", row(125));
        assert!(row(64) > 140, "content should be bright, got {}", row(64));
    }
}
