"""The always-on status panel (above the footer) explains the focused item. The motivating case:
a RED entry in Overview -> 'Archive — mirrored' means a repo could NOT be mirrored; focusing it must
surface why (the failure reason). Also checks the panel describes a focused config field and a failure."""
import json, os, sys, time, pty, select

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_panel/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 9.0, "disk_used_delta": 0, "disk_build_total": 4 << 30, "disk_free": 1 << 30,
    "jobs": 4, "paused": False, "total": 6,
    "counts": {"built": 1, "failed": 1, "building": 1, "queued": 0, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 1}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "",
    "building": [{"slug": "ofl/aaa", "worker": 1, "dur": 3.0, "backend": "fontmake", "note": ""}],
    "failures_recent": [{"slug": "ofl/ccc", "error": "boom", "log": "logs/ccc.log"}],
    "built_recent": [{"slug": "ofl/eee", "backend": "fontmake", "bytes": 1000, "compare": "differ", "log": ""}],
    # first archive entry is a FAILURE with a concrete reason -> panel must show it
    "archive_recent": [
        {"status": "failed", "repo": "owner/gone", "reason": "remote: Repository not found (HTTP 404)"},
        {"status": "added", "repo": "owner/ok"}],
    "cohorts": [], "migration": {}, "op_stats": {}, "phase_durations": {},
    "tasks": [{"key": "build", "name": "build fonts", "status": "running", "elapsed": 9.0,
               "done": 2, "total": 6, "detail": ""}],
    "control_log": [], "dep_relaxations": [],
    "config": {"source": "metadata", "google_fonts": "/gf", "archive": "/a", "build_dir": BUILD,
               "backend": "auto", "fontc_bin": None, "jobs": 4, "percent": 10.0, "timeout": None,
               "populate_archive": True, "manage_venvs": True, "compare": False},
    "config_path": "", "done": False,
}
open(BUILD + "/status.json", "w").write(json.dumps(status))

harness = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
fe = g.CursesFrontend(g.MonitorState("{BUILD}")); fe.monitor = True
fe.run()
print("PANEL_OK", file=sys.stderr); sys.stderr.flush()
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


def snap():
    """Return the most recent screen only (panel reflects current focus, not history)."""
    return out.decode("utf-8", "replace")


TAB, RIGHT, ESC = b"\t", b"\x1bOC", b"\x1b"
drain(1.0)
# land on Configuration tab: the panel should describe the focused config field (first = source)
cfg_txt = snap()
assert "where the worklist comes from" in cfg_txt, "config-field panel missing:\n" + cfg_txt[-700:]
print("panel describes the focused configuration field")

os.write(fd, TAB); drain(0.5)             # Configuration -> Overview
out = b""                                  # clear so we read the post-focus screen
os.write(fd, RIGHT); drain(0.6)           # focus 'Archive — mirrored', first item = the failure
panel_txt = snap()
shows_repo = "owner/gone" in panel_txt and "could NOT be mirrored" in panel_txt
shows_reason = "Repository not found" in panel_txt or "404" in panel_txt
print("panel shows the archive repo + 'could NOT be mirrored':", shows_repo)
print("panel shows the failure reason:", shows_reason)

os.write(fd, b"q")
end = time.time() + 6
while time.time() < end:
    if not drain(0.4):
        break
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)
full = out.decode("utf-8", "replace")
no_crash = "_curses.error" not in full and "Traceback" not in full
print("no crash:", no_crash)
assert shows_repo and shows_reason and no_crash, panel_txt[-800:]
print("\nPTY-STATUS-PANEL OK")
