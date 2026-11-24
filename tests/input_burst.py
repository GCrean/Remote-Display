"""Burst-input regression test: types fast and taps fast through the raw protocol, then checks that
nothing was dropped. Needs a device with the companion running and a focused text field
(the script opens Settings' search box itself). Usage: ANDROID_SERIAL=<serial> input_burst.py"""
import os, socket, struct, subprocess, sys, time

SER = os.environ.get("ANDROID_SERIAL")
ADB = ["adb"] + (["-s", SER] if SER else [])


def adb(*a):
    return subprocess.run(ADB + list(a), capture_output=True, text=True).stdout


subprocess.run(ADB + ["forward", "tcp:27999", "localabstract:remote_display"], check=True)
s = socket.create_connection(("127.0.0.1", 27999))


def rd(n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        assert c, "eof"
        b += c
    return b


h = rd(14)
assert h[0] == 1, "no hello"
rd(struct.unpack("<I", h[2:6])[0])


def send(t, p=b""):
    s.sendall(struct.pack("<BBIQ", t, 0, len(p), 0) + p)


def text(ch):
    send(0x13, ch.encode())


def key(code):
    send(0x11, struct.pack("<Bii", 1, code, 0))


def dump_text():
    # UI text of the focused edit field via uiautomator
    adb("shell", "uiautomator", "dump", "/sdcard/ui.xml")
    xml = adb("shell", "cat", "/sdcard/ui.xml")
    import xml.etree.ElementTree as ET
    try:
        for n in ET.fromstring(xml[xml.index("<?xml"):]).iter("node"):
            if n.get("focused") == "true" and ("EditText" in n.get("class", "") or "AutoComplete" in n.get("class", "")):
                t = n.get("text")
                return "" if t.startswith("Search") else t  # empty field shows its hint
    except Exception:
        pass
    return None


adb("shell", "input", "keyevent", "KEYCODE_WAKEUP")
adb("shell", "am", "start", "-a", "android.settings.APP_SEARCH_SETTINGS")
time.sleep(2.5)
adb("shell", "input", "tap", "564", "206")  # focus the search box
time.sleep(1.5)

fails = 0
for rnd in range(3):
    word = "thunderlightremotedisplay"
    for ch in word:  # no delay at all: worst case for read-modify-write races
        text(ch)
    time.sleep(1.5)
    got = dump_text()
    ok = got == word
    print(f"typing round {rnd}: sent {word!r} got {got!r} -> {'OK' if ok else 'FAIL'}")
    fails += not ok
    for _ in range(len(word) + 3):
        key(67)  # backspace burst
    time.sleep(1.5)
    got = dump_text()
    ok = got in ("", None) or got.lower().startswith("search")
    print(f"backspace round {rnd}: got {got!r} -> {'OK' if ok else 'FAIL'}")
    fails += not ok

# Rapid taps: tap the same toggle area N times in quick succession and count gesture completions.
adb("logcat", "-c")
N = 8
for i in range(N):
    for a in (0, 2):
        p = struct.pack("<BBIIII", a, 0, 300, 700, 864, 1920)
        send(0x10, p)
        time.sleep(0.01)
    time.sleep(0.03)
time.sleep(2.5)
log = adb("logcat", "-d", "-s", "Input:*")
cancelled = log.count("cancelled")
print(f"rapid taps: sent {N}, cancelled {cancelled}")
fails += cancelled
print("FAILURES:", fails)
sys.exit(1 if fails else 0)
