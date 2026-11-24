//! Thin wrapper over the `adb` CLI: device discovery, companion app lifecycle,
//! port forwarding, and shell-level helpers (orientation lock, fallback keys).

use rd_protocol as proto;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub struct AdbError(pub String);
impl std::fmt::Display for AdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AdbError {}
pub type Result<T> = std::result::Result<T, AdbError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    /// "device", "unauthorized", "offline", ...
    pub state: String,
    pub model: String,
}

impl Device {
    pub fn ready(&self) -> bool {
        self.state == "device"
    }
    pub fn label(&self) -> String {
        let m = if self.model.is_empty() { "Android" } else { self.model.as_str() };
        if self.ready() {
            format!("{m} ({})", self.serial)
        } else {
            format!("{m} ({}) [{}]", self.serial, self.state)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    Missing,
    /// Installed with this versionCode (older than required).
    Outdated(u32),
    Current,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrientationMode {
    Auto,
    Portrait,
    Landscape,
    PortraitReverse,
    LandscapeReverse,
}

#[derive(Clone)]
pub struct Adb {
    exe: PathBuf,
}

impl Adb {
    /// Locate adb via `ADB`, `ANDROID_HOME`/`ANDROID_SDK_ROOT`, then `PATH`.
    pub fn locate() -> Result<Adb> {
        let name = if cfg!(windows) { "adb.exe" } else { "adb" };
        if let Ok(p) = std::env::var("ADB") {
            return Ok(Adb { exe: p.into() });
        }
        for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
            if let Ok(root) = std::env::var(var) {
                let p = Path::new(&root).join("platform-tools").join(name);
                if p.exists() {
                    return Ok(Adb { exe: p });
                }
            }
        }
        if Command::new(name).arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() {
            return Ok(Adb { exe: name.into() });
        }
        Err(AdbError("adb not found: install Android platform-tools or set ADB / ANDROID_HOME".into()))
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.exe);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        c
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.cmd().args(args).output().map_err(|e| AdbError(format!("failed to run adb: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(AdbError(format!("adb {} failed: {}{}", args.join(" "), err.trim(), stdout.trim())));
        }
        Ok(stdout)
    }

    pub fn shell(&self, serial: &str, args: &[&str]) -> Result<String> {
        let mut a = vec!["-s", serial, "shell"];
        a.extend_from_slice(args);
        self.run(&a)
    }

    pub fn start_server(&self) -> Result<()> {
        self.run(&["start-server"]).map(|_| ())
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        Ok(parse_devices(&self.run(&["devices", "-l"])?))
    }

    pub fn install_state(&self, serial: &str) -> Result<InstallState> {
        // `pm path` exits 1 with no output when the package is absent, which adb propagates
        // as a failure; only a failure to reach the device at all is a real error.
        let path = match self.shell(serial, &["pm", "path", proto::PACKAGE]) {
            Ok(p) => p,
            Err(e) if e.0.contains("no devices") || e.0.contains("offline") || e.0.contains("unauthorized") || e.0.contains("not found") => return Err(e),
            Err(_) => String::new(),
        };
        if !path.contains("package:") {
            return Ok(InstallState::Missing);
        }
        let dump = self.shell(serial, &["dumpsys", "package", proto::PACKAGE])?;
        Ok(match parse_version_code(&dump) {
            Some(v) if v >= proto::COMPANION_VERSION_CODE => InstallState::Current,
            Some(v) => InstallState::Outdated(v),
            None => InstallState::Outdated(0),
        })
    }

    pub fn install(&self, serial: &str, apk: &Path) -> Result<()> {
        let apk = apk.to_string_lossy();
        // -r replace, -g grant runtime permissions declared in the manifest, -d allow downgrade.
        self.run(&["-s", serial, "install", "-r", "-g", "-d", &apk]).map(|_| ())
    }

    /// Install if missing/outdated. Returns true if an install happened.
    pub fn ensure_installed(&self, serial: &str, apk: Option<&Path>) -> Result<bool> {
        match self.install_state(serial)? {
            InstallState::Current => Ok(false),
            s => {
                let apk = apk.ok_or_else(|| {
                    AdbError(format!(
                        "companion app is {} but no APK was found (build it: `cd android && gradlew assembleRelease`, copy to assets/remote-display.apk)",
                        if s == InstallState::Missing { "not installed" } else { "outdated" }
                    ))
                })?;
                self.install(serial, apk)?;
                Ok(true)
            }
        }
    }

    /// Best-effort permission setup that avoids on-device prompts. Failures are ignored
    /// because OEM builds restrict some of these; the app reports what is still missing.
    pub fn prepare_permissions(&self, serial: &str) {
        // Remove companions installed under a previous package name (best effort).
        for legacy in proto::LEGACY_PACKAGES {
            let _ = self.run(&["-s", serial, "uninstall", legacy]);
        }
        for perm in ["android.permission.RECORD_AUDIO", "android.permission.POST_NOTIFICATIONS"] {
            let _ = self.shell(serial, &["pm", "grant", proto::PACKAGE, perm]);
        }
        let _ = self.shell(serial, &["appops", "set", proto::PACKAGE, "PROJECT_MEDIA", "allow"]);
        let _ = self.enable_accessibility(serial);
    }

    /// Enables the input-injection accessibility service via secure settings
    /// (allowed for the shell user), preserving other enabled services.
    pub fn enable_accessibility(&self, serial: &str) -> Result<()> {
        let cur = self.shell(serial, &["settings", "get", "secure", "enabled_accessibility_services"])?;
        let cur = cur.trim();
        if cur.contains(proto::ACCESSIBILITY_SERVICE) {
            return Ok(());
        }
        let new = if cur.is_empty() || cur == "null" {
            proto::ACCESSIBILITY_SERVICE.to_string()
        } else {
            format!("{cur}:{}", proto::ACCESSIBILITY_SERVICE)
        };
        self.shell(serial, &["settings", "put", "secure", "enabled_accessibility_services", &new])?;
        self.shell(serial, &["settings", "put", "secure", "accessibility_enabled", "1"])?;
        Ok(())
    }

    /// Start the companion activity, which requests screen-capture consent and starts the service.
    pub fn launch(&self, serial: &str, max_size: u32, bitrate: u32) -> Result<()> {
        self.shell(
            serial,
            &[
                "am", "start", "-n", proto::MAIN_ACTIVITY, "--ez", "autostart", "true", "--ei", "max_size",
                &max_size.to_string(), "--ei", "bitrate", &bitrate.to_string(),
            ],
        )
        .map(|_| ())
    }

    pub fn stop_app(&self, serial: &str) {
        let _ = self.shell(serial, &["am", "force-stop", proto::PACKAGE]);
    }

    /// `adb forward tcp:<port> localabstract:remote_display`. Pass 0 for an auto-chosen port.
    pub fn forward(&self, serial: &str, port: u16) -> Result<u16> {
        let local = format!("tcp:{port}");
        let remote = format!("localabstract:{}", proto::ABSTRACT_SOCKET);
        let out = self.run(&["-s", serial, "forward", &local, &remote])?;
        if port == 0 {
            out.trim().parse().map_err(|_| AdbError(format!("unexpected forward output: {out}")))
        } else {
            Ok(port)
        }
    }

    pub fn remove_forward(&self, serial: &str, port: u16) {
        let _ = self.run(&["-s", serial, "forward", "--remove", &format!("tcp:{port}")]);
    }

    /// Fallback for keys the accessibility service cannot inject (arrows, volume, ...).
    pub fn keyevent(&self, serial: &str, keycode: i32) {
        let _ = self.shell(serial, &["input", "keyevent", &keycode.to_string()]);
    }

    pub fn set_orientation(&self, serial: &str, mode: OrientationMode) -> Result<()> {
        let (accel, rot) = match mode {
            OrientationMode::Auto => ("1", None),
            OrientationMode::Portrait => ("0", Some("0")),
            OrientationMode::Landscape => ("0", Some("1")),
            OrientationMode::PortraitReverse => ("0", Some("2")),
            OrientationMode::LandscapeReverse => ("0", Some("3")),
        };
        self.shell(serial, &["settings", "put", "system", "accelerometer_rotation", accel])?;
        if let Some(r) = rot {
            self.shell(serial, &["settings", "put", "system", "user_rotation", r])?;
        }
        Ok(())
    }

    // ---- device control / app management (used by the MCP server) ----

    /// PNG screenshot via `screencap -p` (full resolution, works without streaming).
    pub fn screencap(&self, serial: &str) -> Result<Vec<u8>> {
        let out = self
            .cmd()
            .args(["-s", serial, "exec-out", "screencap", "-p"])
            .output()
            .map_err(|e| AdbError(format!("failed to run adb: {e}")))?;
        if !out.status.success() || out.stdout.len() < 8 {
            return Err(AdbError(format!("screencap failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        Ok(out.stdout)
    }

    /// Real display size in pixels (`wm size`, honouring an override).
    pub fn screen_size(&self, serial: &str) -> Result<(u32, u32)> {
        let out = self.shell(serial, &["wm", "size"])?;
        parse_wm_size(&out).ok_or_else(|| AdbError(format!("unexpected wm size output: {out}")))
    }

    pub fn tap(&self, serial: &str, x: u32, y: u32) -> Result<()> {
        self.shell(serial, &["input", "tap", &x.to_string(), &y.to_string()]).map(|_| ())
    }

    pub fn swipe(&self, serial: &str, x1: u32, y1: u32, x2: u32, y2: u32, ms: u32) -> Result<()> {
        let a = [x1, y1, x2, y2, ms].map(|v| v.to_string());
        self.shell(serial, &["input", "swipe", &a[0], &a[1], &a[2], &a[3], &a[4]]).map(|_| ())
    }

    /// Type text into the focused field (`input text`; spaces are encoded, other chars shell-quoted).
    pub fn input_text(&self, serial: &str, text: &str) -> Result<()> {
        let cmd = format!("input text {}", shell_quote(&text.replace(' ', "%s")));
        self.shell(serial, &[&cmd]).map(|_| ())
    }

    pub fn open_notifications(&self, serial: &str) -> Result<()> {
        self.shell(serial, &["cmd", "statusbar", "expand-notifications"]).map(|_| ())
    }

    /// Installed package names. `third_party_only` excludes system apps.
    pub fn list_packages(&self, serial: &str, third_party_only: bool) -> Result<Vec<String>> {
        let flag = if third_party_only { "-3" } else { "-s" };
        let mut a = vec!["pm", "list", "packages"];
        if third_party_only {
            a.push(flag);
        }
        let mut v: Vec<String> =
            self.shell(serial, &a)?.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_string).collect();
        v.sort();
        Ok(v)
    }

    /// Install an arbitrary APK from the host (`-r` replace, `-g` grant runtime permissions).
    pub fn install_apk(&self, serial: &str, apk: &Path) -> Result<String> {
        if !apk.is_file() {
            return Err(AdbError(format!("APK not found: {}", apk.display())));
        }
        let p = apk.to_string_lossy();
        self.run(&["-s", serial, "install", "-r", "-g", &p]).map(|s| s.trim().to_string())
    }

    pub fn uninstall(&self, serial: &str, package: &str, keep_data: bool) -> Result<String> {
        check_package(package)?;
        let mut a = vec!["-s", serial, "uninstall"];
        if keep_data {
            a.push("-k");
        }
        a.push(package);
        self.run(&a).map(|s| s.trim().to_string())
    }

    pub fn launch_package(&self, serial: &str, package: &str) -> Result<()> {
        check_package(package)?;
        let out = self.shell(serial, &["monkey", "-p", package, "-c", "android.intent.category.LAUNCHER", "1"])?;
        if out.contains("No activities found") {
            return Err(AdbError(format!("{package} has no launchable activity")));
        }
        Ok(())
    }

    pub fn force_stop(&self, serial: &str, package: &str) -> Result<()> {
        check_package(package)?;
        self.shell(serial, &["am", "force-stop", package]).map(|_| ())
    }

    pub fn getprop(&self, serial: &str, prop: &str) -> String {
        self.shell(serial, &["getprop", prop]).unwrap_or_default().trim().to_string()
    }

    /// Foreground window line, e.g. `com.android.settings/.Settings`.
    pub fn focused_window(&self, serial: &str) -> String {
        let out = self.shell(serial, &["dumpsys", "window"]).unwrap_or_default();
        out.lines().find(|l| l.contains("mCurrentFocus")).map(|l| l.trim().to_string()).unwrap_or_default()
    }
}

/// Package names are interpolated into a device shell; accept only the legal charset.
pub fn check_package(p: &str) -> Result<()> {
    if !p.is_empty() && p.len() < 256 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_') {
        Ok(())
    } else {
        Err(AdbError(format!("invalid package name: {p:?}")))
    }
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn parse_wm_size(out: &str) -> Option<(u32, u32)> {
    // "Physical size: 1080x2400" and optionally "Override size: ..." (the override wins).
    let line = out.lines().rev().find(|l| l.contains("size:"))?;
    let (w, h) = line.split("size:").nth(1)?.trim().split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

pub fn parse_devices(out: &str) -> Vec<Device> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?.to_string();
            let state = it.next()?.to_string();
            let model = it.find_map(|t| t.strip_prefix("model:")).unwrap_or("").replace('_', " ");
            Some(Device { serial, state, model })
        })
        .collect()
}

pub fn parse_version_code(dumpsys: &str) -> Option<u32> {
    let i = dumpsys.find("versionCode=")? + "versionCode=".len();
    dumpsys[i..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// Find the bundled APK: `$REMOTE_DISPLAY_APK`, next to the exe, `assets/` in cwd or the workspace.
pub fn find_apk() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("REMOTE_DISPLAY_APK") {
        return Some(p.into());
    }
    let mut dirs: Vec<PathBuf> = vec![];
    if let Ok(exe) = std::env::current_exe() {
        // Dev tree (target/release/..), Windows install dir, macOS `Contents/Resources`,
        // Linux `/usr/lib/<name>` (deb/AppImage resource dirs).
        for a in exe.ancestors().skip(1).take(4) {
            for sub in ["", "assets", "Resources", "resources", "lib/remote-display", "lib/Remote Display", "lib/remote_display"] {
                dirs.push(if sub.is_empty() { a.to_path_buf() } else { a.join(sub) });
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("assets"));
        dirs.push(cwd.join("..").join("assets"));
    }
    dirs.into_iter().map(|d| d.join("remote-display.apk")).find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices() {
        let out = "List of devices attached\nR58M123 device usb:1-1 product:x model:Pixel_7 device:panther transport_id:1\nABC unauthorized usb:1-2 transport_id:2\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].model, "Pixel 7");
        assert!(d[0].ready());
        assert!(!d[1].ready());
    }

    #[test]
    fn wm_size_and_quoting() {
        assert_eq!(parse_wm_size("Physical size: 1080x2400
"), Some((1080, 2400)));
        assert_eq!(parse_wm_size("Physical size: 1080x2400
Override size: 720x1600"), Some((720, 1600)));
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert!(check_package("com.a.b_c").is_ok());
        assert!(check_package("a; rm -rf /").is_err());
    }

    #[test]
    fn parses_version() {
        assert_eq!(parse_version_code("    versionCode=12 minSdk=26 targetSdk=35"), Some(12));
        assert_eq!(parse_version_code("nothing"), None);
    }
}
