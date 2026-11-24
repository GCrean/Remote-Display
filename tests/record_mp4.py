"""Record ~8 s of a live session through MCP and print the resulting file."""
import json, subprocess, sys, time, os, glob
EXE = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "debug", "remote-display.exe")
p = subprocess.Popen([EXE, "--mcp-stdio"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
n = 0
def call(name, **a):
    global n; n += 1
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":n,"method":"tools/call","params":{"name":name,"arguments":a}})+"\n"); p.stdin.flush()
    r = json.loads(p.stdout.readline())["result"]; return r["isError"], r["content"][0].get("text","")
print(call("connect"))
for _ in range(60):
    _, t = call("session_status"); s = json.loads(t)
    if s["phase"] == "streaming" and s["fps"] > 0: break
    time.sleep(1)
print("status:", s["phase"], s["frame_size"], s["audio"])
print(call("launch_app", package=sys.argv[1] if len(sys.argv) > 1 else "com.fourklang.player"))
time.sleep(3)
print(call("start_recording"))
time.sleep(1); call("scroll", direction="down"); time.sleep(3); call("scroll", direction="up"); time.sleep(3)
print(call("stop_recording")); time.sleep(2)
_, t = call("session_status"); print("note:", json.loads(t)["note"])
call("disconnect"); p.stdin.close(); p.wait(timeout=10)
print(glob.glob(os.path.expanduser("~/RemoteDisplay/*")))
