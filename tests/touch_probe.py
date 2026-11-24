"""Send raw touch messages to the companion (no GUI). Usage: touch_probe.py drag|tap"""
import socket, struct, subprocess, sys, time
subprocess.run(["adb", "forward", "tcp:27999", "localabstract:remote_display"], check=True)
s = socket.create_connection(("127.0.0.1", 27999))
def rd(n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b)); assert c, "eof"; b += c
    return b
h = rd(14); assert h[0] == 1, "no hello"; rd(struct.unpack("<I", h[2:6])[0])
def touch(a, x, y): 
    p = struct.pack("<BBIIII", a, 0, x, y, 864, 1920)
    s.sendall(struct.pack("<BBIQ", 0x10, 0, len(p), 0) + p)
mode = sys.argv[1]
if mode == "scroll":
    pl = struct.pack("<IIIIii", 432, 1500, 864, 1920, 0, -250)
    s.sendall(struct.pack("<BBIQ", 0x12, 0, len(pl), 0) + pl)
    time.sleep(1.5); sys.exit()
if mode == "tap":
    touch(0, 432, 1100); time.sleep(0.08); touch(2, 432, 1100)
else:
    touch(0, 432, 1000); time.sleep(0.05)
    for i in range(1, 11):
        touch(1, 432, 1000 - i * 60); time.sleep(0.03)
    touch(2, 432, 400)
time.sleep(1.5)
