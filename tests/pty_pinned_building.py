"""'Now building' is PINNED: shown on every tab (not just overview) while families are compiling,
and not duplicated as an overview section. (We force a full repaint via SIGWINCH on each tab so
curses re-emits the unchanged pinned strip — otherwise its diff output wouldn't include it.)"""
import json, os, sys, time, pty, select, struct, fcntl, termios, signal

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_pin/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 30.0, "disk_used_delta": 0, "disk_build_total": 1 << 30, "disk_free": 1 << 30,
    "jobs": 4, "paused": False, "total": 20,
    "counts": {"built": 1, "failed": 2, "building": 2, "queued": 5, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 1}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "",
    "building": [{"slug": "ofl/buildingnow", "worker": 1, "dur": 12.0, "backend": "fontmake", "note": ""},
                 {"slug": "ofl/secondone", "worker": 2, "dur": 3.0, "backend": "", "note": "pre-build"}],
    "failures_recent": [{"slug": "ofl/f1", "error": "boom", "log": ""}],
    "built_recent": [], "fail_categories": [],
    "archive_recent": [], "cohorts": [{"key": "base", "count": 3, "requirements": "",
                                       "families": ["A", "B", "C"]}],
    "migration": {}, "op_stats": {}, "phase_durations": {},
    "tasks": [{"key": "build", "name": "build", "status": "running", "elapsed": 30,
               "done": 3, "total": 20, "detail": ""}],
    "control_log": [], "dep_relaxations": [], "config_path": "",
    "config": {"build_dir": BUILD, "source": "metadata", "google_fonts": "/gf", "archive": "/a",
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
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 100, 0, 0))
out = b""


def repaint():                                         # force curses to re-emit the whole screen
    for rows in (41, 40):
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 100, 0, 0))
        try:
            os.kill(pid, signal.SIGWINCH)
        except OSError:
            pass
        drain(0.25)


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


TAB = b"\t"
drain(0.9)
overview = out.decode("utf-8", "replace")              # default tab
out = b""
os.write(fd, TAB); drain(0.4); repaint()               # overview -> cohorts (force full repaint)
cohorts = out.decode("utf-8", "replace")
out = b""
os.write(fd, TAB); os.write(fd, TAB); drain(0.4); repaint()   # -> built -> failures
failures = out.decode("utf-8", "replace")
os.write(fd, b"q"); drain(0.8)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)

full = overview + cohorts + failures
no_crash = "_curses.error" not in full and "Traceback" not in full
print("no crash:", no_crash)
# pinned strip appears on the cohorts tab and the failures tab (not just overview)
on_cohorts = "Now building" in cohorts and "ofl/buildingnow" in cohorts
on_failures = "Now building" in failures and "ofl/buildingnow" in failures
print("pinned 'Now building' visible on cohorts tab:", on_cohorts)
print("pinned 'Now building' visible on failures tab:", on_failures)
# the pre-build note is shown for the family running pre-build commands
shows_prebuild = "pre-build" in cohorts or "pre-build" in failures
print("a family's 'pre-build' step is shown:", shows_prebuild)
assert no_crash and on_cohorts and on_failures, full[-900:]
print("\nPTY-PINNED-BUILDING OK")
