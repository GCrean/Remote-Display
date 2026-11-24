# Remote Display

Android screen + audio mirrored to a desktop (Rust + GPUI) with mouse/keyboard control.

## Prerequisites

- Rust (stable), a C/C++ compiler (MSVC / clang / gcc — OpenH264 builds from source).
  Linux also needs the GPUI system deps (Vulkan, wayland/x11, xkbcommon, alsa — see Zed's docs).
- `adb` on `PATH` (or `ADB` / `ANDROID_HOME` set).
- Android phone with USB debugging on. For the (optional) APK build: JDK 17+, Android SDK 35, NDK 26.3 and CMake 3.22.1
  (`sdkmanager "platforms;android-35" "ndk;26.3.11579264" "cmake;3.22.1"`).

## Build & run

```sh
# 1. Companion APK (a prebuilt one is committed at assets/remote-display.apk)
cd android
echo "sdk.dir=/path/to/Android/sdk" > local.properties
./gradlew assembleRelease
cp app/build/outputs/apk/release/app-release.apk ../assets/remote-display.apk

# 2. Desktop
cd ..
cargo run --release -p rd-desktop
```

The companion package is `com.thunderlight.remote.display` (older builds named `dev.remotedisplay.companion` are removed automatically on connect).

If you change anything under `android/`, bump `versionCode` in `android/app/build.gradle.kts`,
`COMPANION_VERSION_CODE` in `crates/protocol/src/lib.rs` and `BuildConfig_VERSION_CODE` in
`Streamer.kt` together so the desktop reinstalls it.

## Using it

1. Plug in the phone, accept the USB-debugging prompt, pick it in the toolbar, click **Connect**.
2. The app installs/updates the companion, grants what it can, launches it, and waits.
3. **Accept the screen-capture dialog on the phone** (Android always asks).
4. Mouse = touch (drag works), wheel = scroll, right click = Back, middle click = Home, Esc = Back,
   typing sends text, Ctrl/Cmd+V pastes. Sidebar: orientation (device), rotate view, record, screenshot, mute. The Android Back/Home/Recents buttons sit under the display.

The preview only ever shows a live session: after Disconnect, an unplugged cable or an error the stage returns to the "No device connected" placeholder and never keeps showing the previous device's last picture.

After **Stop** (recording) and after **Take screenshot** a Save As dialog opens so you choose where the file goes. The file is first written to `~/RemoteDisplay` (or the last folder you saved into), so cancelling the dialog never loses it. Recordings are a single `.mp4` (H.264 video + AAC-LC audio, no external tools needed). If the phone rotates mid-recording the video keeps one constant size: the rotated part is letterboxed into the original canvas. The file is only playable once recording is stopped, so use Stop/Disconnect (closing the window or stdin also finalises it).

Clicks and keys are injected on the phone by the companion's accessibility service. Android switches that
service off whenever the app is updated, so the desktop app re-enables it over adb on **every** connect
(not just on install). If "input: enable accessibility" still shows in the status bar, enable *Remote
Display* under Settings → Accessibility (some OEMs block enabling it over adb).

## MCP server

The desktop app embeds an MCP server (Streamable HTTP) on `http://127.0.0.1:8765/mcp`
(`--mcp-port N` to change, `--no-mcp` to disable, `--headless` to run it with no window).

```sh
# HTTP (talks to the running GUI app, shares its session)
claude mcp add --transport http remote-display http://127.0.0.1:8765/mcp
# stdio (the client spawns its own headless instance; newline-delimited JSON-RPC)
claude mcp add remote-display -- /path/to/remote-display --mcp-stdio
```

With `--mcp-stdio` nothing but protocol messages is written to stdout (logs go to stderr), and
the process exits when stdin closes. A stdio instance has its own session, separate from any GUI.

Tools: `list_devices`, `device_info`, `screenshot`, `tap`, `long_press`, `swipe`, `scroll`,
`type_text`, `press_key`, `set_orientation`, `list_apps`, `install_app`, `uninstall_app`,
`launch_app`, `stop_app`, plus `connect` / `disconnect` / `session_status` /
`start_recording` / `stop_recording` for the live session. Coordinates are real display pixels
(as in `screenshot`). Input and screenshots go through adb, so they work without streaming
or the companion app.

Security: loopback only, browser origins/hosts other than localhost are rejected, and
`REMOTE_DISPLAY_MCP_TOKEN` (if set) is required as `Authorization: Bearer <token>`. There is no
other auth, so anything on this machine that can reach the port can control the device
(including uninstalling apps); set the token or `--no-mcp` on shared machines.

## Installers and releases

The current installers are checked in under `dist/` (Windows `.exe`, macOS Apple-silicon `.dmg`, Linux
`.deb` and `.AppImage`), each bundling the companion APK from `assets/remote-display.apk`. Rebuild and
commit them together with the APK whenever the app changes.

```sh
cargo build --release -p rd-desktop
cargo install cargo-packager --version 0.11.8 --locked      # once
cargo packager --release --formats nsis -c Packager.toml     # Windows installer (dist/*.exe)
cargo packager --release --formats app -c Packager.toml && \
  packaging/make-dmg.sh "dist/Remote Display.app" dist/remote-display_0.2.0_aarch64.dmg   # macOS
cargo packager --release --formats deb,appimage -c Packager.toml                          # Linux
```
