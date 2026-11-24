//! H.264 (Annex-B) -> BGRA decoding via OpenH264 (software, builds from source on all platforms).

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    /// BGRA, tightly packed, already rotated by the view rotation.
    pub bgra: Vec<u8>,
}

pub struct H264 {
    dec: Decoder,
}

impl H264 {
    pub fn new() -> Result<Self, String> {
        Ok(Self { dec: Decoder::new().map_err(|e| e.to_string())? })
    }

    /// Feed one access unit (or SPS/PPS). Returns a picture when one is ready.
    pub fn decode(&mut self, data: &[u8], view_rotation: u8) -> Option<Decoded> {
        let yuv = self.dec.decode(data).ok()??;
        let (w, h) = yuv.dimensions();
        let strides = yuv.strides();
        let bgra = yuv_to_bgra(yuv.y(), yuv.u(), yuv.v(), strides, w, h);
        Some(rotate_bgra(bgra, w, h, view_rotation))
    }
}

/// BT.601 limited-range YUV420 -> BGRA (MediaCodec's default colour description).
fn yuv_to_bgra(y: &[u8], u: &[u8], v: &[u8], (ys, us, vs): (usize, usize, usize), w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![255u8; w * h * 4];
    for row in 0..h {
        let yr = &y[row * ys..row * ys + w];
        let ur = &u[(row / 2) * us..];
        let vr = &v[(row / 2) * vs..];
        let o = &mut out[row * w * 4..(row + 1) * w * 4];
        for col in 0..w {
            let c = (yr[col] as i32 - 16).max(0) * 298;
            let d = ur[col / 2] as i32 - 128;
            let e = vr[col / 2] as i32 - 128;
            let r = (c + 409 * e + 128) >> 8;
            let g = (c - 100 * d - 208 * e + 128) >> 8;
            let b = (c + 516 * d + 128) >> 8;
            let p = &mut o[col * 4..col * 4 + 4];
            p[0] = b.clamp(0, 255) as u8;
            p[1] = g.clamp(0, 255) as u8;
            p[2] = r.clamp(0, 255) as u8;
        }
    }
    out
}

/// Rotate clockwise by `rot` quarter-turns.
fn rotate_bgra(src: Vec<u8>, w: usize, h: usize, rot: u8) -> Decoded {
    let (w32, h32) = (w as u32, h as u32);
    match rot % 4 {
        0 => Decoded { width: w32, height: h32, bgra: src },
        2 => {
            let mut out = vec![0u8; src.len()];
            for i in 0..w * h {
                let j = w * h - 1 - i;
                out[j * 4..j * 4 + 4].copy_from_slice(&src[i * 4..i * 4 + 4]);
            }
            Decoded { width: w32, height: h32, bgra: out }
        }
        r => {
            // Output is h wide, w tall.
            let mut out = vec![0u8; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let (nx, ny) = if r == 1 { (h - 1 - y, x) } else { (y, w - 1 - x) };
                    let (s, d) = ((y * w + x) * 4, (ny * h + nx) * 4);
                    out[d..d + 4].copy_from_slice(&src[s..s + 4]);
                }
            }
            Decoded { width: h32, height: w32, bgra: out }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_90_stacks_left_to_right_as_top_to_bottom() {
        // 2x1 image: A B -> rotated cw it is 1 wide, 2 tall: A over B.
        let src = vec![1, 1, 1, 1, 2, 2, 2, 2];
        let d = rotate_bgra(src, 2, 1, 1);
        assert_eq!((d.width, d.height), (1, 2));
        assert_eq!(&d.bgra[0..4], &[1, 1, 1, 1]);
        assert_eq!(&d.bgra[4..8], &[2, 2, 2, 2]);
    }

    #[test]
    fn rotate_270_puts_right_pixel_on_top() {
        let src = vec![1, 1, 1, 1, 2, 2, 2, 2];
        let d = rotate_bgra(src, 2, 1, 3);
        assert_eq!(&d.bgra[0..4], &[2, 2, 2, 2]);
    }
}
