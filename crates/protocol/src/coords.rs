//! Mapping from a point in the on-screen viewport to a pixel in the video frame.
//!
//! The device streams its display *as rendered*, so after a device rotation the
//! frame itself changes size (1080x1920 -> 1920x1080). The only extra rotation
//! we handle is a purely visual, user-chosen rotation of the viewport
//! (`view_rotation`, clockwise quarter-turns).

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Largest rect with the content's aspect ratio centred in `view` (letterbox/pillarbox).
pub fn fit(view: Rect, content_w: f32, content_h: f32) -> Rect {
    if content_w <= 0.0 || content_h <= 0.0 || view.w <= 0.0 || view.h <= 0.0 {
        return Rect { x: view.x, y: view.y, w: 0.0, h: 0.0 };
    }
    let scale = (view.w / content_w).min(view.h / content_h);
    let (w, h) = (content_w * scale, content_h * scale);
    Rect { x: view.x + (view.w - w) / 2.0, y: view.y + (view.h - h) / 2.0, w, h }
}

/// Size of the frame as displayed after `view_rotation` quarter-turns.
pub fn displayed_size(frame_w: u32, frame_h: u32, view_rotation: u8) -> (f32, f32) {
    if view_rotation % 2 == 1 {
        (frame_h as f32, frame_w as f32)
    } else {
        (frame_w as f32, frame_h as f32)
    }
}

/// Map a window-space point to frame pixels. `None` if it falls in the padding.
pub fn view_to_frame(view: Rect, px: f32, py: f32, frame_w: u32, frame_h: u32, view_rotation: u8) -> Option<(u32, u32)> {
    let (dw, dh) = displayed_size(frame_w, frame_h, view_rotation);
    let r = fit(view, dw, dh);
    if r.w <= 0.0 || px < r.x || py < r.y || px >= r.x + r.w || py >= r.y + r.h {
        return None;
    }
    Some(clamped_map(r, px, py, frame_w, frame_h, view_rotation))
}

/// Like [`view_to_frame`] but clamps into the frame (used during drags that leave the video).
pub fn view_to_frame_clamped(view: Rect, px: f32, py: f32, frame_w: u32, frame_h: u32, view_rotation: u8) -> (u32, u32) {
    let (dw, dh) = displayed_size(frame_w, frame_h, view_rotation);
    let r = fit(view, dw, dh);
    if r.w <= 0.0 {
        return (0, 0);
    }
    clamped_map(r, px, py, frame_w, frame_h, view_rotation)
}

fn clamped_map(r: Rect, px: f32, py: f32, fw: u32, fh: u32, rot: u8) -> (u32, u32) {
    let u = ((px - r.x) / r.w).clamp(0.0, 1.0);
    let v = ((py - r.y) / r.h).clamp(0.0, 1.0);
    // Invert the clockwise rotation applied for display.
    let (x, y) = match rot % 4 {
        0 => (u, v),
        1 => (v, 1.0 - u),
        2 => (1.0 - u, 1.0 - v),
        _ => (1.0 - v, u),
    };
    let ix = ((x * fw as f32) as u32).min(fw.saturating_sub(1));
    let iy = ((y * fh as f32) as u32).min(fh.saturating_sub(1));
    (ix, iy)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: Rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 500.0 };

    #[test]
    fn pillarbox_padding_is_rejected() {
        // portrait 1080x1920 in a 1000x500 view -> content is 281x500 centred.
        assert_eq!(view_to_frame(V, 10.0, 250.0, 1080, 1920, 0), None);
        assert!(view_to_frame(V, 500.0, 250.0, 1080, 1920, 0).is_some());
    }

    #[test]
    fn centre_maps_to_centre() {
        let (x, y) = view_to_frame(V, 500.0, 250.0, 1080, 1920, 0).unwrap();
        assert!((x as i32 - 540).abs() <= 1 && (y as i32 - 960).abs() <= 1);
    }

    #[test]
    fn rotation_90_maps_top_left_of_view_to_bottom_left_of_frame() {
        // View rotated 90° cw: frame's bottom-left appears at the displayed top-left.
        let v = Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 };
        let (x, y) = view_to_frame(v, 1.0, 1.0, 1080, 1920, 1).unwrap();
        assert!(x <= 1 && y >= 1918, "{x},{y}");
    }

    #[test]
    fn rotation_180_flips_both() {
        let v = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let (x, y) = view_to_frame(v, 1.0, 1.0, 100, 100, 2).unwrap();
        assert!(x >= 98 && y >= 98);
    }

    #[test]
    fn rotation_270_inverse_of_90() {
        let v = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let a = view_to_frame(v, 10.0, 20.0, 100, 100, 1).unwrap();
        // 90 then 270 are mirror images through the centre on both axes.
        let b = view_to_frame(v, 90.0, 80.0, 100, 100, 3).unwrap();
        assert!((a.0 as i32 - b.0 as i32).abs() <= 1 && (a.1 as i32 - b.1 as i32).abs() <= 1, "{a:?} {b:?}");
    }
}
