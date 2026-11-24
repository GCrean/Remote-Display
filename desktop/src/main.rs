// No console window on Windows. CLI modes (--mcp-stdio/--headless) re-attach to the parent
// console (or use redirected pipes) in `attach_console`.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod assets;
mod audio;
mod decoder;
mod mcp;
mod recorder;
mod session;
mod transcode;

use gpui::{prelude::*, *};
use rd_adbkit::OrientationMode;
use rd_protocol::coords::{fit, view_to_frame, view_to_frame_clamped, Rect};
use rd_protocol::{AudioStatus, HostMsg, NavAction, TouchAction};
use session::{Cmd, Controller, Phase, SaveKind};
use std::path::Path;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

// ---- palette (dark, slate + indigo) -----------------------------------------------------------
const BG: u32 = 0x0b0d12;
const SIDEBAR: u32 = 0x10131a;
const CARD: u32 = 0x171b24;
const CARD_HOVER: u32 = 0x1e2330;
const BORDER: u32 = 0x232937;
const TEXT: u32 = 0xe8eaf0;
const MUTED: u32 = 0x8b93a5;
const FAINT: u32 = 0x5b6377;
const ACCENT: u32 = 0x6366f1;
const ACCENT_HOVER: u32 = 0x7477f5;
const GREEN: u32 = 0x22c55e;
const AMBER: u32 = 0xf59e0b;
const RED: u32 = 0xef4444;
const WHITE: u32 = 0xffffff;

struct RemoteDisplay {
    ctl: Controller,
    focus: FocusHandle,
    image: Option<Arc<RenderImage>>,
    video_bounds: Rc<Cell<Bounds<Pixels>>>,
    dragging: bool,
    orientation: OrientationMode,
    saving: bool,
}

impl RemoteDisplay {
    fn new(ctl: Controller, cx: &mut Context<Self>) -> Self {
        let shared = ctl.shared.clone();
        // Repaint when the background threads publish something new.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(8)).await;
            let dirty = shared.dirty.swap(false, Ordering::AcqRel);
            let alive = this.update(cx, |this, cx| {
                if dirty {
                    cx.notify();
                }
                this.process_saves(cx);
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
        Self {
            ctl,
            focus: cx.focus_handle(),
            image: None,
            video_bounds: Rc::new(Cell::new(Bounds::default())),
            dragging: false,
            orientation: OrientationMode::Auto,
            saving: false,
        }
    }

    fn rotation(&self) -> u8 {
        self.ctl.shared.view_rotation.load(Ordering::Relaxed)
    }

    /// (unrotated frame size) from the currently displayed (view-rotated) picture.
    fn raw_frame_size(&self) -> Option<(u32, u32)> {
        let (w, h) = self.ctl.shared.snapshot().frame_size;
        if w == 0 || h == 0 {
            return None;
        }
        Some(if self.rotation() % 2 == 1 { (h, w) } else { (w, h) })
    }

    fn view_rect(&self) -> Rect {
        let b = self.video_bounds.get();
        Rect { x: f32::from(b.origin.x), y: f32::from(b.origin.y), w: f32::from(b.size.width), h: f32::from(b.size.height) }
    }

    fn map(&self, pos: Point<Pixels>, clamp: bool) -> Option<(u32, u32, u32, u32)> {
        let (fw, fh) = self.raw_frame_size()?;
        let (px, py) = (f32::from(pos.x), f32::from(pos.y));
        let r = self.view_rect();
        let rot = self.rotation();
        let (x, y) = if clamp { view_to_frame_clamped(r, px, py, fw, fh, rot) } else { view_to_frame(r, px, py, fw, fh, rot)? };
        Some((x, y, fw, fh))
    }

    fn touch(&self, action: TouchAction, pos: Point<Pixels>, clamp: bool) -> bool {
        match self.map(pos, clamp) {
            Some((x, y, fw, fh)) => {
                self.ctl.shared.send_input(HostMsg::Touch { action, pointer: 0, x, y, frame_w: fw, frame_h: fh });
                true
            }
            None => false,
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        let shared = &self.ctl.shared;
        if m.control || m.platform {
            if k.key == "v" {
                if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    shared.send_input(HostMsg::Text(t));
                }
            }
            return;
        }
        // Keys the companion handles itself (accessibility service).
        match k.key.as_str() {
            "escape" => return shared.send_input(HostMsg::Nav(NavAction::Back)),
            "home" => return shared.send_input(HostMsg::Nav(NavAction::Home)),
            "backspace" => return shared.send_input(HostMsg::Key { down: true, keycode: 67, meta: 0 }),
            "enter" => return shared.send_input(HostMsg::Key { down: true, keycode: 66, meta: 0 }),
            // GPUI doesn't always attach a key_char to space, so it would fall through and be dropped.
            "space" => return shared.send_input(HostMsg::Text(" ".into())),
            _ => {}
        }
        if let Some(ch) = k.key_char.as_ref().filter(|s| !s.is_empty() && !s.chars().any(|c| c.is_control())) {
            return shared.send_input(HostMsg::Text(ch.clone()));
        }
        // Everything else goes through `adb shell input keyevent`.
        let code = match k.key.as_str() {
            "up" => 19,
            "down" => 20,
            "left" => 21,
            "right" => 22,
            "tab" => 61,
            "delete" => 112,
            "pageup" => 92,
            "pagedown" => 93,
            "end" => 123,
            _ => return,
        };
        self.ctl.send(Cmd::KeyFallback(code));
    }

    /// Save As dialog for each finished recording/screenshot. The file already exists in the
    /// working folder, so cancelling loses nothing; saving moves it and remembers the folder.
    fn process_saves(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(req) = self.ctl.shared.save_requests.lock().unwrap().pop_front() else { return };
        self.saving = true;
        let dir = req.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = req.path.file_name().map(|n| n.to_string_lossy().into_owned());
        let rx = cx.prompt_for_new_path(&dir, name.as_deref());
        let shared = self.ctl.shared.clone();
        cx.spawn(async move |this, cx| {
            let chosen = rx.await.ok().and_then(|r| r.ok()).flatten();
            let note = match chosen {
                Some(mut dest) if dest != req.path => {
                    if dest.extension().is_none() {
                        if let Some(ext) = req.path.extension() {
                            dest.set_extension(ext);
                        }
                    }
                    match move_file(&req.path, &dest) {
                        Ok(()) => {
                            if let Some(dir) = dest.parent().map(Path::to_path_buf) {
                                shared.update_settings(|s| match req.kind {
                                    SaveKind::Recording => s.recordings_dir = dir,
                                    SaveKind::Screenshot => s.screenshots_dir = dir,
                                });
                            }
                            format!("Saved to {}", dest.display())
                        }
                        Err(e) => format!("Could not save to {} ({e}); kept {}", dest.display(), req.path.display()),
                    }
                }
                _ => format!("Kept in {}", req.path.display()),
            };
            shared.update(|s| s.note = note);
            this.update(cx, |this, cx| {
                this.saving = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn refresh_image(&mut self, window: &mut Window) {
        let Some(f) = self.ctl.shared.frame.lock().unwrap().take() else { return };
        let Some(buf) = image::RgbaImage::from_raw(f.width, f.height, f.bgra) else { return };
        // gpui's atlas expects BGRA; the buffer already is, only the type label says RGBA.
        let new = Arc::new(RenderImage::new(vec![image::Frame::new(buf)]));
        if let Some(old) = self.image.replace(new) {
            let _ = window.drop_image(old);
        }
    }
}

// ---- small view helpers -----------------------------------------------------------------------

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Different volume: copy then remove.
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

fn icon(path: &'static str, size: f32, color: u32) -> Svg {
    svg().path(path).size(px(size)).flex_none().text_color(rgb(color))
}

fn section_label(text: &'static str) -> Div {
    div().px_1().pb_2().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(rgb(FAINT)).child(text)
}

fn dot(color: u32) -> Div {
    div().size(px(8.)).flex_none().rounded_full().bg(rgb(color))
}

/// Square icon button used in groups (nav bar, sidebar toggles).
fn icon_button(id: impl Into<ElementId>, path: &'static str, enabled: bool) -> Stateful<Div> {
    let b = div()
        .id(id)
        .size(px(44.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(BORDER));
    let b = if enabled {
        b.cursor_pointer().hover(|s| s.bg(rgb(CARD_HOVER))).active(|s| s.bg(rgb(BORDER)))
    } else {
        b.opacity(0.4)
    };
    b.child(icon(path, 20., if enabled { TEXT } else { MUTED }))
}

/// Full-width action row used for toggles in the sidebar.
fn row_button(id: impl Into<ElementId>, path: &'static str, label: impl Into<SharedString>, active: Option<u32>, enabled: bool) -> Stateful<Div> {
    let tint = active.unwrap_or(TEXT);
    let b = div()
        .id(id)
        .h(px(38.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .rounded_lg()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(if active.is_some() { tint } else { BORDER }))
        .text_sm()
        .text_color(rgb(if enabled { tint } else { MUTED }));
    let b = if enabled { b.cursor_pointer().hover(|s| s.bg(rgb(CARD_HOVER))) } else { b.opacity(0.5) };
    b.child(icon(path, 16., if enabled { tint } else { MUTED })).child(label.into())
}

impl Render for RemoteDisplay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_image(window);
        let st = self.ctl.shared.snapshot();
        let streaming = st.phase == Phase::Streaming;
        // Only a live session may show a picture: after a disconnect, an unplugged device or an
        // error, drop the last frame so the stage shows the placeholder, not the previous device.
        if !streaming {
            if let Some(old) = self.image.take() {
                let _ = window.drop_image(old);
            }
            self.ctl.shared.frame.lock().unwrap().take();
        }
        let busy = matches!(st.phase, Phase::Preparing(_) | Phase::WaitingForDevice);
        let muted = self.ctl.shared.muted.load(Ordering::Relaxed);

        // -- status text/colour
        let (status_color, status_title, status_detail) = match &st.phase {
            Phase::Idle => (FAINT, "Not connected".to_string(), "Select a device and press Connect".to_string()),
            Phase::Preparing(s) => (AMBER, "Connecting".to_string(), format!("{s}…")),
            Phase::WaitingForDevice => (AMBER, "Waiting for the phone".to_string(), "Accept the screen-capture prompt on the device".to_string()),
            Phase::Streaming => (
                GREEN,
                "Live".to_string(),
                format!("{} · {}×{} · {} fps", st.device_name, st.frame_size.0, st.frame_size.1, st.fps),
            ),
            Phase::Error(e) => (RED, "Connection problem".to_string(), e.clone()),
        };

        // -- device cards
        let mut devices = div().flex().flex_col().gap_2();
        if st.devices.is_empty() {
            devices = devices.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .gap_3()
                    .items_center()
                    .child(icon("icons/usb.svg", 22., FAINT))
                    .child(div().text_sm().text_color(rgb(MUTED)).child("No devices found. Connect via USB and enable USB debugging.")),
            );
        }
        for (i, d) in st.devices.iter().enumerate() {
            let selected = st.selected.as_deref() == Some(d.serial.as_str());
            let serial = d.serial.clone();
            let ready = d.ready();
            let title = if d.model.is_empty() { "Android device".to_string() } else { d.model.clone() };
            let sub = if ready { d.serial.clone() } else { format!("{} · {}", d.serial, d.state) };
            devices = devices.child(
                div()
                    .id(("dev", i))
                    .p_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(if selected { ACCENT } else { BORDER }))
                    .bg(if selected { rgba(0x6366f11f) } else { rgba(0x171b24ff) })
                    .when(ready, |s| s.cursor_pointer().hover(|s| s.bg(rgb(CARD_HOVER))))
                    .when(!ready, |s| s.opacity(0.6))
                    .on_click(cx.listener(move |this, _, _, _| {
                        if ready {
                            this.ctl.send(Cmd::Select(serial.clone()));
                        }
                    }))
                    .child(
                        div()
                            .size(px(34.))
                            .flex_none()
                            .rounded_lg()
                            .bg(rgba(0xffffff0d))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("icons/phone.svg", 18., if selected { ACCENT_HOVER } else { MUTED })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).text_color(rgb(TEXT)).truncate().child(title))
                            .child(div().text_xs().text_color(rgb(MUTED)).truncate().child(sub)),
                    )
                    .child(dot(if ready { GREEN } else { AMBER })),
            );
        }

        // -- connect / disconnect
        let connect = if streaming || busy {
            div()
                .id("disconnect")
                .h(px(42.))
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .rounded_lg()
                .border_1()
                .border_color(rgb(RED))
                .text_color(rgb(RED))
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0xef44441f)))
                .on_click(cx.listener(|this, _, _, _| this.ctl.send(Cmd::Disconnect)))
                .child(icon("icons/plug.svg", 16., RED))
                .child(if busy { "Cancel" } else { "Disconnect" })
        } else {
            let can = st.selected.is_some();
            div()
                .id("connect")
                .h(px(42.))
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .rounded_lg()
                .bg(rgb(ACCENT))
                .text_color(rgb(WHITE))
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .when(can, |s| s.cursor_pointer().hover(|s| s.bg(rgb(ACCENT_HOVER))))
                .when(!can, |s| s.opacity(0.45))
                .on_click(cx.listener(move |this, _, _, _| {
                    if can {
                        this.ctl.send(Cmd::Connect)
                    }
                }))
                .child(icon("icons/plug.svg", 16., WHITE))
                .child("Connect")
        };

        // -- orientation segmented control
        let seg = |id: &'static str, label: &'static str, mode: OrientationMode, cx: &mut Context<Self>, current: OrientationMode| {
            let on = current == mode;
            div()
                .id(id)
                .flex_1()
                .h(px(32.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .text_color(rgb(if on { TEXT } else { MUTED }))
                .bg(if on { rgba(0x6366f13a) } else { rgba(0x00000000) })
                .hover(|s| s.text_color(rgb(TEXT)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.orientation = mode;
                    this.ctl.send(Cmd::Orientation(mode));
                    cx.notify();
                }))
                .child(label)
        };
        let orient = div()
            .flex()
            .p_1()
            .gap_1()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(seg("o-auto", "Auto", OrientationMode::Auto, cx, self.orientation))
            .child(seg("o-port", "Portrait", OrientationMode::Portrait, cx, self.orientation))
            .child(seg("o-land", "Landscape", OrientationMode::Landscape, cx, self.orientation));

        let rot_deg = self.rotation() as u32 * 90;
        let body = div()
            .id("sidebar-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_5()
            // brand
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(38.))
                            .rounded_xl()
                            .bg(rgb(ACCENT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("icons/logo.svg", 22., WHITE)),
                    )
                    .child(
                        div()
                            .child(div().text_base().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child("Remote Display"))
                            .child(div().text_xs().text_color(rgb(MUTED)).child("Android screen & control")),
                    ),
            )
            // devices
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(section_label("DEVICES"))
                            .child(
                                div()
                                    .id("refresh")
                                    .size(px(26.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(CARD)))
                                    .on_click(cx.listener(|this, _, _, _| this.ctl.send(Cmd::Refresh)))
                                    .child(icon("icons/refresh.svg", 15., MUTED)),
                            ),
                    )
                    .child(devices),
            )
            .child(connect)
            // display
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(section_label("DISPLAY"))
                    .child(orient)
                    .child(div().h(px(8.)))
                    .child(
                        row_button("o-view", "icons/rotate.svg", format!("Rotate view · {rot_deg}°"), None, true).on_click(cx.listener(|this, _, _, cx| {
                            let s = &this.ctl.shared;
                            s.view_rotation.store((s.view_rotation.load(Ordering::Relaxed) + 1) % 4, Ordering::Relaxed);
                            cx.notify();
                        })),
                    ),
            )
            // session
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_label("SESSION"))
                    .child(
                        row_button(
                            "rec",
                            if st.recording { "icons/stop.svg" } else { "icons/record.svg" },
                            if st.recording { "Stop recording" } else { "Record" },
                            Some(if st.recording { RED } else { TEXT }).filter(|_| st.recording),
                            streaming,
                        )
                        .on_click(cx.listener(|this, _, _, _| {
                            let on = !this.ctl.shared.snapshot().recording;
                            this.ctl.send(Cmd::Record { on, prompt: true });
                        })),
                    )
                    .child(
                        row_button("shot", "icons/camera.svg", "Take screenshot", None, st.selected.is_some()).on_click(cx.listener(|this, _, _, _| {
                            if this.ctl.shared.snapshot().selected.is_some() {
                                this.ctl.send(Cmd::Screenshot);
                            }
                        })),
                    )
                    .child(
                        row_button(
                            "mute",
                            if muted { "icons/mute.svg" } else { "icons/volume.svg" },
                            if muted { "Unmute audio" } else { "Mute audio" },
                            None,
                            st.audio == AudioStatus::Streaming,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            let m = &this.ctl.shared.muted;
                            m.store(!m.load(Ordering::Relaxed), Ordering::Relaxed);
                            cx.notify();
                        })),
                    ),
            )
            ;
        let footer = (
                div()
                    .flex_none()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(dot(status_color))
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(status_title)),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(status_detail))
                    .when(streaming, |s| {
                        s.child(div().text_xs().text_color(rgb(FAINT)).child(format!(
                            "{}{}",
                            st.audio.label(),
                            if st.input_ready { "" } else { " · enable accessibility for input" }
                        )))
                    })
                    .when(!st.note.is_empty(), |s| s.child(div().pt_1().text_xs().text_color(rgb(FAINT)).line_clamp(2).child(st.note.clone())))
        );
        let sidebar = div()
            .w(px(300.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(body)
            .child(footer);

        // -- video stage -------------------------------------------------------------------------
        let image = self.image.clone();
        let has_image = image.is_some();
        let bounds_cell = self.video_bounds.clone();
        let frame_dims = image.as_ref().map(|i| {
            let s = i.size(0);
            (s.width.0 as f32, s.height.0 as f32)
        });
        let video = canvas(
            move |bounds, _, _| {
                bounds_cell.set(bounds);
                bounds
            },
            move |_, bounds, window, _| {
                if let (Some(img), Some((w, h))) = (image, frame_dims) {
                    let r = fit(
                        Rect { x: f32::from(bounds.origin.x), y: f32::from(bounds.origin.y), w: f32::from(bounds.size.width), h: f32::from(bounds.size.height) },
                        w,
                        h,
                    );
                    let b = Bounds::new(point(px(r.x), px(r.y)), size(px(r.w), px(r.h)));
                    let radius = px(18.);
                    // Frame: backing + hairline border, then the picture clipped to rounded corners.
                    window.paint_quad(quad(b.dilate(px(1.)), radius + px(1.), rgb(0x000000), px(1.), rgba(0xffffff22), BorderStyle::Solid));
                    let _ = window.paint_image(b, Corners::all(radius), img, 0, false);
                }
            },
        )
        .size_full();

        let placeholder = div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .size(px(84.))
                    .rounded_full()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon("icons/phone.svg", 38., if busy { AMBER } else { FAINT })),
            )
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(if busy {
                "Connecting…"
            } else if matches!(st.phase, Phase::Error(_)) {
                "Couldn't connect"
            } else {
                "No device connected"
            }))
            .child(
                div()
                    .max_w(px(360.))
                    .text_center()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(match &st.phase {
                        Phase::WaitingForDevice => "Accept the screen-capture prompt on your phone.".to_string(),
                        Phase::Error(e) => e.clone(),
                        Phase::Preparing(_) => "Installing and starting the companion app.".to_string(),
                        _ => "Plug in your phone with USB debugging on, choose it on the left and press Connect.".to_string(),
                    }),
            );

        let stage = div()
            .id("video")
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, _| {
                    window.focus(&this.focus);
                    this.dragging = this.touch(TouchAction::Down, ev.position, false);
                }),
            )
            .on_mouse_down(MouseButton::Right, cx.listener(|this, _: &MouseDownEvent, _, _| this.ctl.shared.send_input(HostMsg::Nav(NavAction::Back))))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, _: &MouseDownEvent, _, _| this.ctl.shared.send_input(HostMsg::Nav(NavAction::Home))))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, _| {
                if this.dragging && ev.pressed_button == Some(MouseButton::Left) {
                    this.touch(TouchAction::Move, ev.position, true);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _, _| {
                    if std::mem::take(&mut this.dragging) {
                        this.touch(TouchAction::Up, ev.position, true);
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _, _| {
                    if std::mem::take(&mut this.dragging) {
                        this.touch(TouchAction::Up, ev.position, true);
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, _| {
                if let Some((x, y, fw, fh)) = this.map(ev.position, false) {
                    let d = ev.delta.pixel_delta(px(24.));
                    this.ctl.shared.send_input(HostMsg::Scroll { x, y, frame_w: fw, frame_h: fh, dx: f32::from(d.x) as i32, dy: f32::from(d.y) as i32 });
                }
            }))
            .child(video)
            .when(!has_image, |s| s.child(placeholder));

        // -- Android navigation bar under the display
        let nav = div()
            .flex_none()
            .h(px(64.))
            .flex()
            .items_center()
            .justify_center()
            .gap_4()
            .child(icon_button("nav-back", "icons/back.svg", streaming).on_click(cx.listener(|this, _, _, _| this.ctl.shared.send_input(HostMsg::Nav(NavAction::Back)))))
            .child(icon_button("nav-home", "icons/home.svg", streaming).on_click(cx.listener(|this, _, _, _| this.ctl.shared.send_input(HostMsg::Nav(NavAction::Home)))))
            .child(icon_button("nav-recents", "icons/recents.svg", streaming).on_click(cx.listener(|this, _, _, _| this.ctl.shared.send_input(HostMsg::Nav(NavAction::Recents)))));

        let main = div().flex_1().min_w_0().h_full().flex().flex_col().px_6().pt_6().pb_2().child(stage).child(nav);

        div()
            .size_full()
            .flex()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_key(ev, cx)))
            .child(sidebar)
            .child(main)
    }
}

/// With the GUI subsystem there is no console; CLI modes print to the parent's console if any
/// (redirected pipes, e.g. an MCP client spawning `--mcp-stdio`, work without this).
#[cfg(windows)]
fn attach_console() {
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn GetStdHandle(which: u32) -> isize;
        fn SetStdHandle(which: u32, handle: isize) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(which);
            if h == 0 || h == -1 {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
                    use std::os::windows::io::IntoRawHandle;
                    SetStdHandle(which, f.into_raw_handle() as isize);
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    if flag("--mcp-stdio") || flag("--headless") || flag("--help") {
        attach_console();
    }
    if flag("--help") {
        println!("remote-display [--mcp-port N] [--no-mcp] [--headless] [--mcp-stdio]");
        return;
    }
    let port = args
        .iter()
        .position(|a| a == "--mcp-port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(mcp::DEFAULT_PORT);

    let ctl = Controller::spawn();
    if flag("--mcp-stdio") {
        // MCP over stdin/stdout for clients that spawn the process; nothing else may touch stdout.
        mcp::serve_stdio(&ctl);
        ctl.shutdown(); // finalise any recording before the process exits
        return;
    }
    if !flag("--no-mcp") {
        match mcp::spawn(ctl.clone(), port) {
            Ok(p) => eprintln!("MCP server: http://127.0.0.1:{p}/mcp"),
            Err(e) => eprintln!("MCP server disabled (port {port}): {e}"),
        }
    }
    if flag("--headless") {
        // No window: serve MCP (and sessions started through it) until killed.
        loop {
            std::thread::park();
        }
    }

    let shutdown_ctl = ctl.clone();
    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1180.), px(920.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(860.), px(560.))),
                titlebar: Some(TitlebarOptions { title: Some("Remote Display".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| RemoteDisplay::new(ctl, cx));
                window.focus(&view.read(cx).focus);
                view
            },
        )
        .unwrap();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
    shutdown_ctl.shutdown();
}
