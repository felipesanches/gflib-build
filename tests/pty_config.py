"""The Configuration tab (5) renders, edits a field (+/space), and applying (a) writes a
control.json the daemon would pick up. Drives CursesFrontend in a pty."""
import json, os, sys, time, pty, select

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_config/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 5.0, "disk_used_delta": 0, "disk_free": 1 << 30, "jobs": 4, "paused": False,
    "total": 10, "counts": {"built": 2, "failed": 0, "building": 1, "queued": 7, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 2}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "", "building": [], "failures_recent": [], "cohorts": [],
    "migration": {}, "op_stats": {}, "phase_durations": {},
    "tasks": [{"key": "build", "name": "build fonts", "status": "running", "elapsed": 5.0,
               "done": 2, "total": 10, "detail": ""}],
    "archive_recent": [], "control_log": [],
    "config": {"source": "metadata", "google_fonts": "/gf", "archive": "/a", "build_dir": BUILD,
               "backend": "auto", "fontc_bin": None, "jobs": 4, "percent": 10.0, "timeout": None,
               "populate_archive": True, "manage_venvs": True, "compare": False, "only": ""},
    "done": False,
}
open(BUILD + "/status.json", "w").write(json.dumps(status))

harness = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
ms = g.MonitorState("{BUILD}"); fe = g.CursesFrontend(ms); fe.monitor = True
fe.run()
print("CFG_OK", file=sys.stderr); sys.stderr.flush()
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
# config is the default (leftmost) tab; bump percent (the first/selected field) +5 +5 = 20; apply.
# (selection-by-arrow is covered elsewhere; multi-byte arrow keys split unreliably under a
#  non-blocking pty getch, so this focuses on the edit+apply path on the default field.)
for seq in (b"1", b"+", b"+", b"a"):
    os.write(fd, seq)
    drain(0.25)
drain(0.5)
os.write(fd, b"q")
end = time.time() + 8
while time.time() < end:
    if not drain(0.4):
        break
os.waitpid(pid, 0)
ran = b"CFG_OK" in out
ctl = {}
try:
    ctl = json.loads(open(BUILD + "/control.json").read())
except Exception as e:
    print("no control.json:", e)
print("rendered ok:", ran, " control.json:", ctl)
st = ctl.get("set", {})
# percent 10 -> +5 +5 = 20
assert ran, "config tab crashed"
assert st.get("percent") == 20, st
print("PTY-CONFIG OK (config tab edits + apply wrote control.json)")
