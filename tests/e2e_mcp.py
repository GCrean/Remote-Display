"""End-to-end test: drives the desktop app over MCP (stdio, then HTTP) against a real device.

Usage: python tests/e2e_mcp.py [path/to/remote-display(.exe)]
Requires one authorised adb device (or set ANDROID_SERIAL when several are attached). Installs, launches and uninstalls the companion app.
"""
import base64, json, os, re, struct, subprocess, sys, time, urllib.request, glob

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "debug", "remote-display.exe" if os.name == "nt" else "remote-display")
PKG = "com.thunderlight.remote.display"
results = []


def check(name, cond, detail=""):
    results.append((name, bool(cond)))
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail and not cond else ""), flush=True)
    return cond


def adb(*a):
    return subprocess.run(["adb", *a], capture_output=True, text=True).stdout


class Stdio:
    def __init__(self):
        self.p = subprocess.Popen([EXE, "--mcp-stdio"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        self.n = 0

    def rpc(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            self.n += 1
            msg["id"] = self.n
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        if notify:
            return None
        line = self.p.stdout.readline()
        return json.loads(line)

    def call(self, name, **args):
        r = self.rpc("tools/call", {"name": name, "arguments": args})["result"]
        return r["isError"], r["content"]

    def text(self, name, **args):
        err, c = self.call(name, **args)
        return err, (c[0].get("text", "") if c else "")

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


def png_size(b):
    return struct.unpack(">II", b[16:24]) if b[:4] == b"\x89PNG" else None


def tap_text(m, label):
    """Find a UI element by text with uiautomator and tap it through MCP."""
    adb("shell", "uiautomator", "dump", "/sdcard/ui.xml")
    xml = adb("exec-out", "cat", "/sdcard/ui.xml")
    for mt in re.finditer(r'text="([^"]*)"[^>]*?bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', xml):
        if mt.group(1).lower() == label.lower():
            x1, y1, x2, y2 = map(int, mt.groups()[1:])
            m.call("tap", x=(x1 + x2) // 2, y=(y1 + y2) // 2)
            return True
    return False


def main():
    devs = [l.split()[0] for l in adb("devices").splitlines()[1:] if l.strip().endswith("device")]
    want = os.environ.get("ANDROID_SERIAL")
    if want:
        devs = [want] if want in devs else []
    if not check("one adb device (or ANDROID_SERIAL set to a ready one)", len(devs) == 1, str(devs)):
        return
    adb("shell", "pm", "uninstall", PKG)

    m = Stdio()
    r = m.rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}})
    check("stdio initialize", r["result"]["serverInfo"]["name"] == "remote-display")
    m.rpc("notifications/initialized", notify=True)  # must produce no reply line
    tools = m.rpc("tools/list")["result"]["tools"]
    check("stdio tools/list >= 20 tools", len(tools) >= 20, str(len(tools)))
    check("stdio ping after notification stays in sync", m.rpc("ping")["result"] == {})
    check("stdio unknown method -> -32601", m.rpc("nope")["error"]["code"] == -32601)

    err, t = m.text("device_info")
    info = json.loads(t)
    check("device_info", not err and info["screen"][0] > 0, t)

    err, c = m.call("screenshot")
    png = base64.b64decode(c[0]["data"]) if not err else b""
    check("screenshot is PNG at screen size", png_size(png) == tuple(sorted(info["screen"])) or list(png_size(png) or []) == info["screen"], str(png_size(png)))

    m.call("press_key", key="home")
    m.call("launch_app", package="com.android.settings")
    time.sleep(2)
    err, t = m.text("device_info")
    check("launch_app brings Settings to foreground", "settings" in json.loads(t)["focus"].lower(), t)
    m.call("press_key", key="home")
    time.sleep(1)
    err, t = m.text("device_info")
    check("press_key home returns to launcher", "launcher" in json.loads(t)["focus"].lower(), t)
    m.call("stop_app", package="com.android.settings")

    err, t = m.text("install_app", apk_path=os.path.join(ROOT, "assets", "remote-display.apk"))
    check("install_app", not err and "Success" in t, t)
    err, t = m.text("list_apps", filter="thunderlight.remote")
    check("list_apps shows companion", PKG in t, t)
    err, t = m.text("uninstall_app", package="bad; reboot")
    check("uninstall_app rejects injection", err)

    # ---- live session through MCP: install/permissions/launch/consent/stream ----
    err, t = m.text("connect")
    check("connect accepted", not err, t)
    state = {}
    accepted = False
    deadline = time.time() + 90
    while time.time() < deadline:
        _, t = m.text("session_status")
        state = json.loads(t)
        if state["phase"] == "streaming" and state["fps"] > 0:
            break
        if "waiting" in state["phase"] and not accepted:
            time.sleep(1.5)
            # Dialog button text differs by Android version.
            for label in ("Start now", "Allow", "Start", "Share entire screen", "Entire screen"):
                if tap_text(m, label):
                    accepted = True
                    print(f"  (tapped '{label}' on device)", flush=True)
                    break
        time.sleep(1)
    check("session reaches streaming with decoded frames", state.get("phase") == "streaming" and state.get("fps", 0) > 0, json.dumps(state))
    check("frame size reported", state.get("frame_size", [0])[0] > 0, json.dumps(state))
    check("remote input (accessibility) enabled", state.get("remote_input_enabled") is True, json.dumps(state))
    print("  status:", json.dumps(state), flush=True)

    if state.get("phase") == "streaming":
        err, t = m.text("start_recording")
        check("start_recording", not err, t)
        m.call("scroll", direction="down")
        time.sleep(4)
        err, t = m.text("stop_recording")
        time.sleep(3)
        files = [f for f in glob.glob(os.path.join(os.path.expanduser("~"), "RemoteDisplay", "session-*")) if time.time() - os.path.getmtime(f) < 120]
        mp4 = [f for f in files if f.endswith(".mp4")]
        check("recording is a single .mp4 (no .h264/.wav leftovers)", len(mp4) == 1 and len(files) == 1, str(files))
        if mp4:
            data = open(mp4[0], "rb").read()
            check("mp4 has ftyp first and a moov index", data[4:8] == b"ftyp" and b"moov" in data[-200000:], str(data[:12]))
            check("mp4 has H.264 and AAC tracks", b"avc1" in data and b"mp4a" in data)
            check("mp4 non-trivial size", len(data) > 10_000, str(len(data)))
        _, t = m.text("session_status")
        check("recording flag cleared", json.loads(t)["recording"] is False, t)
        m.text("disconnect")
        time.sleep(1)
        _, t = m.text("session_status")
        check("disconnect -> idle", json.loads(t)["phase"] == "idle", t)

    err, t = m.text("uninstall_app", package=PKG)
    check("uninstall_app", not err and "Success" in t, t)
    _, t = m.text("list_apps", filter="thunderlight.remote")
    check("companion gone", PKG not in t, t)
    m.close()
    check("stdio process exits on stdin close", m.p.returncode is not None)

    # ---- HTTP transport parity ----
    p = subprocess.Popen([EXE, "--headless", "--mcp-port", "8799"], stderr=subprocess.DEVNULL)
    try:
        time.sleep(2)
        def http(method, params=None):
            req = urllib.request.Request("http://127.0.0.1:8799/mcp", json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}).encode(), {"Content-Type": "application/json"})
            return json.load(urllib.request.urlopen(req, timeout=20))
        check("http tools/list", len(http("tools/list")["result"]["tools"]) == len(tools))
        r = http("tools/call", {"name": "device_info", "arguments": {}})["result"]
        check("http device_info", json.loads(r["content"][0]["text"])["serial"] == devs[0])
    finally:
        p.kill()


try:
    main()
finally:
    bad = [n for n, ok in results if not ok]
    print(f"\n{len(results) - len(bad)}/{len(results)} passed")
    sys.exit(1 if bad else 0)
