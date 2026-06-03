"""Short-terminal layout: the always-on status panel reserves rows at the bottom, so on a tiny
screen (18 rows) the body must not draw over it. Regression for two review findings:
  - the focused overflowing section's '(+N more)' hint must stay inside the body (not be clobbered
    by the panel separator);
  - the Configuration tab's fields scroll to keep the active one visible and the action button
    stays on-screen (never hidden behind the panel)."""
import json, os, sys, time, pty, select, struct, fcntl, termios

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_short/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 9.0, "disk_used_delta": 0, "disk_build_total": 4 << 30, "disk_free": 1 << 30,
    "jobs": 4, "paused": False, "total": 30,
    "counts": {"built": 0, "failed": 20, "building": 0, "queued": 10, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 0}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "", "building": [],
    "failures_recent": [{"slug": f"ofl/fam{i}", "error": f"boom{i}", "log": ""} for i in range(20)],
    "built_recent": [], "archive_recent": [], "cohorts": [], "migration": {}, "op_stats": {},
    "phase_durations": {},
    "tasks": [{"key": "build", "name": "build", "status": "running", "elapsed": 9.0,
               "done": 0, "total": 30, "detail": ""}],
    "control_log": [], "dep_relaxations": [], "config_path": "",
    "config": {"source": "metadata", "google_fonts": "/gf", "archive": "/a", "build_dir": BUILD,
               "backend": "auto", "fontc_bin": None, "jobs": 4, "percent": 10.0, "timeout": None,
               "populate_archive": True, "manage_venvs": True, "compare": False},
    "done": False,
}
open(BUILD + "/status.json", "w").write(json.dumps(status))

harness = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
fe = g.CursesFrontend(g.MonitorState("{BUILD}")); fe.monitor = True
fe.run()
'''
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execvp(sys.executable, [sys.executable, "-c", harness])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 18, 90, 0, 0))   # short: 18 rows
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
                return
            if not c:
                return
            out += c


RIGHT, TAB = b"\x1bOC", b"\t"
drain(0.9)
cfg = out.decode("utf-8", "replace")                  # Configuration tab on a short screen
out = b""
os.write(fd, TAB); drain(0.4)                          # -> overview
os.write(fd, RIGHT); os.write(fd, RIGHT); os.write(fd, RIGHT); drain(0.6)  # focus Recent failures (sel=0)
ov = out.decode("utf-8", "replace")
os.write(fd, b"q"); drain(1.2)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)

no_crash = "_curses.error" not in (cfg + ov) and "Traceback" not in (cfg + ov)
print("no crash on 18-row screen:", no_crash)
cfg_action = "Start build" in cfg or "apply changes" in cfg
cfg_panel = any(s in cfg for s in ("worklist comes from", "where all build assets", "auto = fontc",
                                   "edit with"))
print("config: action button on-screen:", cfg_action, "| config: panel help line:", cfg_panel)
ov_hint = "more)" in ov                                # the '(+N more)' overflow affordance survived
ov_panel = "FAILED:" in ov                             # the panel's failure description rendered
print("overview: '+N more' hint shown:", ov_hint, "| overview: panel line shown:", ov_panel)
assert no_crash and cfg_action and cfg_panel and ov_hint and ov_panel, (cfg + ov)[-900:]
print("\nPTY-SHORT-TERMINAL OK")
