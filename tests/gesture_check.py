"""Does a gesture change the screen? Compares the content area (ignores the clock)."""
import subprocess, io, time, socket, struct, hashlib
from PIL import Image
def content():
    im = Image.open(io.BytesIO(subprocess.run(["adb","exec-out","screencap","-p"],capture_output=True).stdout)).convert("L")
    return hashlib.md5(im.crop((0,400,1080,2200)).tobytes()).hexdigest()
def reset():
    subprocess.run(["adb","shell","am","force-stop","com.android.settings"]); subprocess.run(["adb","shell","input","keyevent","3"]); time.sleep(1)
    subprocess.run(["adb","shell","am","start","-n","com.android.settings/.Settings"],capture_output=True); time.sleep(2.5)
subprocess.run(["adb","forward","tcp:27999","localabstract:remote_display"],check=True)
s = socket.create_connection(("127.0.0.1",27999))
def rd(n):
    b=b""
    while len(b)<n: c=s.recv(n-len(b)); assert c; b+=c
    return b
h=rd(14); rd(struct.unpack("<I",h[2:6])[0])
def send(t,pl): s.sendall(struct.pack("<BBIQ",t,0,len(pl),0)+pl)
def touch(a,x,y): send(0x10,struct.pack("<BBIIII",a,0,x,y,864,1920))
def drag(x,y0,y1,steps,delay):
    touch(0,x,y0); time.sleep(0.05)
    for i in range(1,steps+1): touch(1,x,y0+(y1-y0)*i//steps); time.sleep(delay)
    touch(2,x,y1)
tests = {
 "CONTROL no-op (expect no change)": lambda: None,
 "adb swipe 300px/300ms": lambda: subprocess.run(["adb","shell","input","swipe","540","1500","540","1200","300"]),
 "protocol scroll": lambda: send(0x12,struct.pack("<IIIIii",432,1000,864,1920,0,-200)),
 "protocol drag 10 steps/30ms": lambda: drag(432,1000,400,10,0.03),
 "protocol drag 30 steps/20ms": lambda: drag(432,1000,400,30,0.02),
}
for name,fn in tests.items():
    reset(); a=content(); fn(); time.sleep(2.0)
    print("SCROLLED" if content()!=a else "no change", "-", name, flush=True)
