"""Section navigation: in a multi-section tab (overview), ←/→ move the focused section (the ▼
marker), ↑/↓ navigate items within it, ↵ opens the focused section's item detail."""
import json, os, sys, time, pty, select

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_sections/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 9.0, "disk_used_delta": 0, "disk_free": 1 << 30, "jobs": 4, "paused": False,
    "total": 6, "counts": {"built": 2, "failed": 2, "building": 2, "queued": 0, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 2}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "",
    "building": [{"slug": "ofl/aaa", "worker": 1, "dur": 3.0, "backend": "fontmake", "note": ""},
                 {"slug": "ofl/bbb", "worker": 2, "dur": 1.0, "backend": "", "note": "checkout"}],
    "failures_recent": [{"slug": "ofl/ccc", "error": "boom one", "log": "logs/ccc.log"},
                        {"slug": "ofl/ddd", "error": "boom two", "log": "logs/ddd.log"}],
    "built_recent": [{"slug": "ofl/eee", "backend": "fontmake", "bytes": 1000, "compare": "differ", "log": ""}],
    "cohorts": [], "migration": {}, "op_stats": {}, "phase_durations": {},
    "tasks": [{"key": "build", "name": "build fonts", "status": "running", "elapsed": 9.0,
               "done": 2, "total": 6, "detail": ""}],
    "archive_recent": [{"status": "added", "repo": "o/r1"}], "control_log": [], "dep_relaxations": [],
    "config": {}, "config_path": "", "done": False,
}
open(BUILD + "/status.json", "w").write(json.dumps(status))

harness = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
fe = g.CursesFrontend(g.MonitorState("{BUILD}")); fe.monitor = True
fe.run()
print("SEC_OK", file=sys.stderr); sys.stderr.flush()
'''
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execvp(sys.executable, [sys.executable, "-c", harness])
out = b""


def drain(sec):
    global out
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                c = os.read(fd, 65536)
            except OSError:
                return False
            if not c:
                return False
            out += c
    return True


TAB, RIGHT, DOWN, ENTER, ESC = b"\t", b"\x1bOC", b"\x1bOB", b"\r", b"\x1b"
drain(1.0)
drain(0.4)             # default tab is now overview
# overview sections (Now building is pinned, not a section): Pipeline · Archive · Recent failures
os.write(fd, RIGHT); drain(0.4)           # focus Archive
os.write(fd, RIGHT); drain(0.4)           # focus Recent failures
os.write(fd, DOWN); drain(0.3)            # select 2nd failure
os.write(fd, ENTER); drain(0.5)           # open its detail
os.write(fd, ESC); drain(0.3)
os.write(fd, b"q");
end = time.time() + 6
while time.time() < end:
    if not drain(0.4):
        break
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)
txt = out.decode("utf-8", "replace")
no_crash = "_curses.error" not in txt and "Traceback" not in txt
print("no crash:", no_crash)
# focus moved off the first section (▼ on a non-Pipeline section appeared)
moved = "▼ Now building" in txt or "▼ Recent failures" in txt or "▼ Archive" in txt
print("←/→ moved section focus:", moved)
# the failures-section detail opened (a failure's detail shows "Failed:")
opened = "Failed:" in txt
print("↵ opened the focused section's item detail:", opened)
assert no_crash and moved and opened, txt[-600:]
print("\nPTY-SECTIONS OK")
