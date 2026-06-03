"""Pressing C in the dashboard must return "reconfigure" (which main() turns into a re-exec
back to the setup wizard). Drives CursesFrontend in a pty and checks the return value."""
import json, os, sys, time, pty, select

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_reconfig/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 1.0, "disk_used_delta": 0, "disk_free": 1 << 30, "jobs": 4, "paused": False,
    "total": 1, "counts": {"built": 0, "failed": 0, "building": 1, "queued": 0, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 0}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "",
    "building": [{"slug": "ofl/x", "worker": 1, "dur": 1.0, "backend": "", "note": "checkout"}],
    "failures_recent": [], "cohorts": [], "migration": {}, "op_stats": {}, "phase_durations": {},
    "tasks": [{"key": "build", "name": "build fonts", "status": "running", "elapsed": 1.0,
               "done": 0, "total": 1, "detail": ""}],
    "archive_recent": [], "done": False,
}
open(BUILD + "/status.json", "w").write(json.dumps(status))

harness = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
ms = g.MonitorState("{BUILD}"); fe = g.CursesFrontend(ms); fe.monitor = True
r = fe.run()
print("RET=" + repr(r), file=sys.stderr); sys.stderr.flush()
'''

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execvp(sys.executable, [sys.executable, "-c", harness])

out = b""


def drain(seconds):
    global out
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                return False
            if not chunk:
                return False
            out += chunk
    return True


drain(1.2)
os.write(fd, b"C")                       # press C -> back to the wizard
end = time.time() + 8
while time.time() < end:
    if not drain(0.4):
        break
os.waitpid(pid, 0)
txt = out.decode("utf-8", "replace")
ok = "RET='reconfigure'" in txt
print("C -> reconfigure:", ok)
sys.exit(0 if ok else 1)
