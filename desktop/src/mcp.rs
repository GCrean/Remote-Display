//! Embedded MCP server (Streamable HTTP transport, JSON responses, no SSE).
//!
//! Listens on 127.0.0.1 only. Optional bearer token via `REMOTE_DISPLAY_MCP_TOKEN`.
//! Device input/screenshots go through adb (real display pixels, works without streaming);
//! `connect`/`disconnect`/recording drive the same session the GUI uses.

use crate::session::{Cmd, Controller, Phase};
use rd_adbkit::{Adb, OrientationMode};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 8765;
const MAX_BODY: usize = 1 << 20;

pub fn spawn(ctl: Controller, port: u16) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    std::thread::Builder::new().name("mcp".into()).spawn(move || {
        let token = std::env::var("REMOTE_DISPLAY_MCP_TOKEN").ok().filter(|t| !t.is_empty());
        for conn in listener.incoming().flatten() {
            let (ctl, token) = (ctl.clone(), token.clone());
            std::thread::spawn(move || {
                let _ = handle_conn(conn, &ctl, token.as_deref());
            });
        }
    })?;
    Ok(port)
}

/// stdio transport: newline-delimited JSON-RPC on stdin/stdout (logs must go to stderr).
/// Returns when stdin closes.
pub fn serve_stdio(ctl: &Controller) {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(items)) => {
                let rs: Vec<Value> = items.iter().filter_map(|m| dispatch(m, ctl)).collect();
                if rs.is_empty() { None } else { Some(Value::Array(rs)) }
            }
            Ok(msg) => dispatch(&msg, ctl),
            Err(e) => Some(err(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(v) = reply {
            // serde_json never emits raw newlines, so one message == one line.
            if writeln!(out, "{v}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

fn handle_conn(stream: TcpStream, ctl: &Controller, token: Option<&str>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut out = stream.try_clone()?;
    let mut r = BufReader::new(stream);

    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());

    let (mut len, mut origin, mut auth, mut host) = (0usize, None, None, None);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let v = v.trim().to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.parse().unwrap_or(0),
                "origin" => origin = Some(v),
                "authorization" => auth = Some(v),
                "host" => host = Some(v),
                _ => {}
            }
        }
    }

    // DNS-rebinding / browser-CSRF protection: only loopback hosts and origins.
    let is_local = |s: &str| {
        let h = s.trim_start_matches("http://").trim_start_matches("https://");
        let h = h.split('/').next().unwrap_or("");
        let h = h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h);
        matches!(h, "127.0.0.1" | "localhost" | "[::1]")
    };
    if origin.as_deref().is_some_and(|o| !is_local(o)) || host.as_deref().is_some_and(|h| !is_local(h)) {
        return respond(&mut out, 403, "Forbidden", "text/plain", "forbidden origin");
    }
    if let Some(t) = token {
        if auth.as_deref() != Some(&format!("Bearer {t}")) {
            return respond(&mut out, 401, "Unauthorized", "text/plain", "missing or bad bearer token");
        }
    }
    if path.split('?').next() != Some("/mcp") {
        return respond(&mut out, 404, "Not Found", "text/plain", "MCP endpoint is /mcp");
    }
    if method != "POST" {
        return respond(&mut out, 405, "Method Not Allowed", "text/plain", "POST JSON-RPC to /mcp");
    }
    if len > MAX_BODY {
        return respond(&mut out, 413, "Payload Too Large", "text/plain", "body too large");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;

    let reply = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Array(items)) => {
            let rs: Vec<Value> = items.iter().filter_map(|m| dispatch(m, ctl)).collect();
            if rs.is_empty() { None } else { Some(Value::Array(rs)) }
        }
        Ok(msg) => dispatch(&msg, ctl),
        Err(e) => Some(err(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    match reply {
        Some(v) => respond(&mut out, 200, "OK", "application/json", &v.to_string()),
        None => respond(&mut out, 202, "Accepted", "text/plain", ""),
    }
}

fn respond(out: &mut TcpStream, code: u16, text: &str, ctype: &str, body: &str) -> std::io::Result<()> {
    write!(out, "HTTP/1.1 {code} {text}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    out.flush()
}

fn err(id: Value, code: i32, msg: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":msg}})
}

/// Returns None for notifications (no id).
fn dispatch(msg: &Value, ctl: &Controller) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let ok = |r: Value| Some(json!({"jsonrpc":"2.0","id":id,"result":r}));
    match method {
        "initialize" => {
            let ver = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-03-26");
            ok(json!({
                "protocolVersion": ver,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "remote-display", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Control a USB-connected Android device: screenshots, tap/swipe/type/keys, install/uninstall/launch apps. Coordinates are real display pixels as in `screenshot`."
            }))
        }
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            ok(match call(name, &args, ctl) {
                Ok(content) => json!({"content": content, "isError": false}),
                Err(e) => json!({"content": [{"type":"text","text": e}], "isError": true}),
            })
        }
        _ => Some(err(id, -32601, &format!("method not found: {method}"))),
    }
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    let mut p = props.as_object().cloned().unwrap_or_default();
    p.insert("serial".into(), json!({"type":"string","description":"Device serial; defaults to the selected/only ready device"}));
    json!({"name": name, "description": desc, "inputSchema": {"type":"object","properties": p, "required": required}})
}

fn tools() -> Vec<Value> {
    let int = |d: &str| json!({"type":"integer","description": d});
    let s = |d: &str| json!({"type":"string","description": d});
    vec![
        tool("list_devices", "List adb devices and their state.", json!({}), &[]),
        tool("device_info", "Model, Android version, screen size and foreground window.", json!({}), &[]),
        tool("screenshot", "Capture the screen as a PNG image (full resolution).", json!({}), &[]),
        tool("tap", "Tap at pixel coordinates.", json!({"x": int("x pixel"), "y": int("y pixel")}), &["x", "y"]),
        tool("long_press", "Press and hold at pixel coordinates.", json!({"x": int("x"), "y": int("y"), "duration_ms": int("default 800")}), &["x", "y"]),
        tool("swipe", "Swipe between two points.", json!({"x1": int("start x"), "y1": int("start y"), "x2": int("end x"), "y2": int("end y"), "duration_ms": int("default 300")}), &["x1", "y1", "x2", "y2"]),
        tool("scroll", "Scroll the screen content in a direction (finger moves opposite, like a mouse wheel).", json!({"direction": {"type":"string","enum":["up","down","left","right"]}, "amount": {"type":"number","description":"fraction of the screen, 0.1-0.9, default 0.5"}}), &["direction"]),
        tool("type_text", "Type text into the focused field (ASCII works best; '%' is not supported).", json!({"text": s("text to type")}), &["text"]),
        tool("press_key", "Press a key: home, back, recents, enter, delete, tab, escape, menu, search, power, wake, volume_up, volume_down, up, down, left, right, notifications, or a numeric Android keycode.", json!({"key": s("key name or keycode")}), &["key"]),
        tool("set_orientation", "Set device orientation.", json!({"mode": {"type":"string","enum":["auto","portrait","landscape","portrait_reverse","landscape_reverse"]}}), &["mode"]),
        tool("list_apps", "List installed package names (third-party only by default).", json!({"include_system": {"type":"boolean"}, "filter": s("substring filter")}), &[]),
        tool("install_app", "Install an APK from a path on this computer (replaces an existing install, grants runtime permissions).", json!({"apk_path": s("host path to .apk")}), &["apk_path"]),
        tool("uninstall_app", "Uninstall an app by package name. Destructive: deletes app data unless keep_data is true.", json!({"package": s("e.g. com.example.app"), "keep_data": {"type":"boolean"}}), &["package"]),
        tool("launch_app", "Launch an app by package name.", json!({"package": s("package name")}), &["package"]),
        tool("stop_app", "Force-stop an app by package name.", json!({"package": s("package name")}), &["package"]),
        tool("connect", "Start the live mirroring session in the desktop app (installs/launches the companion; the user must accept the capture prompt on the device).", json!({}), &[]),
        tool("disconnect", "Stop the live mirroring session.", json!({}), &[]),
        tool("session_status", "State of the mirroring session (phase, fps, audio, recording).", json!({}), &[]),
        tool("start_recording", "Start recording the live session (requires an active session).", json!({}), &[]),
        tool("stop_recording", "Stop recording the live session.", json!({}), &[]),
    ]
}

type Content = Vec<Value>;

fn text(s: impl Into<String>) -> Result<Content, String> {
    Ok(vec![json!({"type":"text","text": s.into()})])
}

fn u(args: &Value, k: &str) -> Result<u32, String> {
    args.get(k).and_then(Value::as_f64).filter(|v| *v >= 0.0).map(|v| v as u32).ok_or_else(|| format!("missing or invalid '{k}'"))
}

fn st<'a>(args: &'a Value, k: &str) -> Result<&'a str, String> {
    args.get(k).and_then(Value::as_str).ok_or_else(|| format!("missing or invalid '{k}'"))
}

fn keycode(name: &str) -> Option<i32> {
    if let Ok(n) = name.parse::<i32>() {
        return Some(n);
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "home" => 3,
        "back" => 4,
        "up" => 19,
        "down" => 20,
        "left" => 21,
        "right" => 22,
        "volume_up" => 24,
        "volume_down" => 25,
        "power" => 26,
        "tab" => 61,
        "enter" => 66,
        "delete" | "backspace" => 67,
        "menu" => 82,
        "search" => 84,
        "escape" => 111,
        "recents" => 187,
        "wake" => 224,
        _ => return None,
    })
}

fn call(name: &str, args: &Value, ctl: &Controller) -> Result<Content, String> {
    let adb = Adb::locate().map_err(|e| e.to_string())?;
    let e = |x: rd_adbkit::AdbError| x.to_string();

    // Session tools don't need a specific adb serial lookup first.
    match name {
        "list_devices" => {
            let d = adb.devices().map_err(e)?;
            return text(if d.is_empty() { "no devices".into() } else { d.iter().map(|d| format!("{} [{}] {}", d.serial, d.state, d.model)).collect::<Vec<_>>().join("\n") });
        }
        "connect" => {
            let serial = pick_serial(&adb, args, ctl)?;
            ctl.send(Cmd::Select(serial.clone()));
            ctl.send(Cmd::Connect);
            return text(format!("connecting to {serial}; poll session_status (the user may need to accept the capture prompt on the device)"));
        }
        "disconnect" => {
            ctl.send(Cmd::Disconnect);
            return text("disconnected");
        }
        "session_status" => {
            let s = ctl.shared.snapshot();
            let phase = match &s.phase {
                Phase::Idle => "idle".to_string(),
                Phase::Preparing(p) => format!("preparing: {p}"),
                Phase::WaitingForDevice => "waiting for the user to accept the capture prompt on the device".into(),
                Phase::Streaming => "streaming".into(),
                Phase::Error(x) => format!("error: {x}"),
            };
            return text(json!({"phase": phase, "device": s.device_name, "frame_size": [s.frame_size.0, s.frame_size.1], "fps": s.fps,
                "audio": s.audio.label(), "remote_input_enabled": s.input_ready, "recording": s.recording, "note": s.note}).to_string());
        }
        "start_recording" | "stop_recording" => {
            if ctl.shared.snapshot().phase != Phase::Streaming {
                return Err("no active session; call connect first".into());
            }
            ctl.send(Cmd::Record { on: name == "start_recording", prompt: false });
            return text(name.replace('_', " "));
        }
        _ => {}
    }

    let serial = pick_serial(&adb, args, ctl)?;
    let sv = serial.as_str();
    match name {
        "device_info" => {
            let (w, h) = adb.screen_size(sv).map_err(e)?;
            text(json!({"serial": sv, "model": adb.getprop(sv, "ro.product.model"), "android": adb.getprop(sv, "ro.build.version.release"),
                "sdk": adb.getprop(sv, "ro.build.version.sdk"), "screen": [w, h], "focus": adb.focused_window(sv)}).to_string())
        }
        "screenshot" => {
            let png = adb.screencap(sv).map_err(e)?;
            Ok(vec![json!({"type":"image","data": base64(&png),"mimeType":"image/png"})])
        }
        "tap" => adb.tap(sv, u(args, "x")?, u(args, "y")?).map_err(e).and_then(|_| text("ok")),
        "long_press" => {
            let (x, y) = (u(args, "x")?, u(args, "y")?);
            adb.swipe(sv, x, y, x, y, u(args, "duration_ms").unwrap_or(800)).map_err(e)?;
            text("ok")
        }
        "swipe" => {
            adb.swipe(sv, u(args, "x1")?, u(args, "y1")?, u(args, "x2")?, u(args, "y2")?, u(args, "duration_ms").unwrap_or(300)).map_err(e)?;
            text("ok")
        }
        "scroll" => {
            let (w, h) = adb.screen_size(sv).map_err(e)?;
            let amt = args.get("amount").and_then(Value::as_f64).unwrap_or(0.5).clamp(0.1, 0.9) as f32;
            let (cx, cy) = (w / 2, h / 2);
            let (dx, dy) = ((w as f32 * amt / 2.0) as u32, (h as f32 * amt / 2.0) as u32);
            // Content scrolls in `direction`, so the finger moves the opposite way.
            let (x1, y1, x2, y2) = match st(args, "direction")? {
                "down" => (cx, cy + dy, cx, cy - dy),
                "up" => (cx, cy - dy, cx, cy + dy),
                "right" => (cx + dx, cy, cx - dx, cy),
                "left" => (cx - dx, cy, cx + dx, cy),
                d => return Err(format!("invalid direction '{d}'")),
            };
            adb.swipe(sv, x1, y1, x2, y2, 300).map_err(e)?;
            text("ok")
        }
        "type_text" => adb.input_text(sv, st(args, "text")?).map_err(e).and_then(|_| text("ok")),
        "press_key" => {
            let k = st(args, "key")?;
            if k.eq_ignore_ascii_case("notifications") {
                adb.open_notifications(sv).map_err(e)?;
            } else {
                adb.shell(sv, &["input", "keyevent", &keycode(k).ok_or_else(|| format!("unknown key '{k}'"))?.to_string()]).map_err(e)?;
            }
            text("ok")
        }
        "set_orientation" => {
            let m = match st(args, "mode")? {
                "auto" => OrientationMode::Auto,
                "portrait" => OrientationMode::Portrait,
                "landscape" => OrientationMode::Landscape,
                "portrait_reverse" => OrientationMode::PortraitReverse,
                "landscape_reverse" => OrientationMode::LandscapeReverse,
                x => return Err(format!("invalid mode '{x}'")),
            };
            adb.set_orientation(sv, m).map_err(e)?;
            text("ok")
        }
        "list_apps" => {
            let all = adb.list_packages(sv, !args.get("include_system").and_then(Value::as_bool).unwrap_or(false)).map_err(e)?;
            let f = args.get("filter").and_then(Value::as_str).unwrap_or("").to_lowercase();
            text(all.into_iter().filter(|p| p.to_lowercase().contains(&f)).collect::<Vec<_>>().join("\n"))
        }
        "install_app" => text(adb.install_apk(sv, Path::new(st(args, "apk_path")?)).map_err(e)?),
        "uninstall_app" => text(adb.uninstall(sv, st(args, "package")?, args.get("keep_data").and_then(Value::as_bool).unwrap_or(false)).map_err(e)?),
        "launch_app" => adb.launch_package(sv, st(args, "package")?).map_err(e).and_then(|_| text("launched")),
        "stop_app" => adb.force_stop(sv, st(args, "package")?).map_err(e).and_then(|_| text("stopped")),
        other => Err(format!("unknown tool '{other}'")),
    }
}

fn pick_serial(adb: &Adb, args: &Value, ctl: &Controller) -> Result<String, String> {
    if let Some(s) = args.get("serial").and_then(Value::as_str) {
        return Ok(s.to_string());
    }
    let devices = adb.devices().map_err(|e| e.to_string())?;
    let ready: Vec<_> = devices.iter().filter(|d| d.ready()).collect();
    // ANDROID_SERIAL (the standard adb variable) disambiguates when several devices are attached.
    if let Ok(want) = std::env::var("ANDROID_SERIAL") {
        if ready.iter().any(|d| d.serial == want) {
            return Ok(want);
        }
    }
    if let Some(sel) = ctl.shared.snapshot().selected {
        if ready.iter().any(|d| d.serial == sel) {
            return Ok(sel);
        }
    }
    match ready.as_slice() {
        [d] => Ok(d.serial.clone()),
        [] => Err("no ready device (check USB debugging authorisation)".into()),
        _ => Err("multiple devices attached; pass 'serial'".into()),
    }
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn keys() {
        assert_eq!(keycode("home"), Some(3));
        assert_eq!(keycode("66"), Some(66));
        assert_eq!(keycode("nope"), None);
    }
}
