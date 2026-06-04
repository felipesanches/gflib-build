"""The cohorts tab shows the ● 'cached venv' marker, and the failures tab shows the persistent
'Failure history' section — both rendered from the snapshot without crashing."""
import json, os, sys, time, pty, select, struct, fcntl, termios, signal

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_persist/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 30.0, "disk_build_total": 1 << 30, "disk_free": 1 << 30, "jobs": 4, "paused": False,
    "total": 6, "counts": {"built": 1, "failed": 1, "building": 0, "queued": 1, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 1}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "", "building": [], "queued_list": [],
    "failures_recent": [{"slug": "ofl/now", "error": "current boom", "log": ""}],
    "built_recent": [],
    "fail_categories": [{"cat": "build error", "count": 1, "hint": "open the log", "families": ["ofl/now"]}],
    "cohorts": [{"key": "c-cached", "count": 2, "requirements": "fontmake", "families": ["Alpha", "Beta"],
                 "cached": True},
                {"key": "c-fresh", "count": 1, "requirements": "x", "families": ["Gamma"], "cached": False}],
    "failure_history": [
        {"ts": 1700000000.0, "slug": "ofl/oldfail", "cause": "missing system library",
         "error": "needs libcairo2-dev", "backend": "fontmake"},
        {"ts": 1700000500.0, "slug": "ofl/now", "cause": "build error", "error": "current boom", "backend": "fontc"}],
    "op_stats": {}, "phase_durations": {}, "migration": {}, "tasks": [], "archive_recent": [],
    "control_log": [], "dep_relaxations": [], "config_path": "", "done": False,
    "config": {"jobs": 4, "percent": 100.0, "build_dir": BUILD, "source": "metadata"},
}
open(BUILD + "/status.json", "w").write(json.dumps(status))

harness = f'''
import sys; sys.path.insert(0,"{REPO}")
import gflib_build as g
fe=g.CursesFrontend(g.MonitorState("{BUILD}")); fe.monitor=True; fe.run()
'''
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execvp(sys.executable, [sys.executable, "-c", harness])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 110, 0, 0))
out = b""


def drain(sec):
    global out
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                ch = os.read(fd, 65536)
            except OSError:
                return
            if not ch:
                return
            out += ch


def repaint():
    for rows in (41, 40):
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 110, 0, 0))
        try:
            os.kill(pid, signal.SIGWINCH)
        except OSError:
            pass
        drain(0.25)


TAB = b"\t"
drain(0.9)
for _ in range(2):                                                   # overview -> queue -> cohorts
    os.write(fd, TAB); drain(0.2)
repaint()
cohorts = out.decode("utf-8", "replace")
out = b""
for _ in range(3):                                                   # -> archive -> built -> failures
    os.write(fd, TAB); drain(0.2)
repaint()
failures = out.decode("utf-8", "replace")
os.write(fd, b"q"); drain(0.6)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)

full = cohorts + failures
no_crash = "_curses.error" not in full and "Traceback" not in full
print("no crash:", no_crash)
shows_cached = "●" in cohorts and "c-cached" in cohorts
print("cohorts tab shows the ● cached marker:", shows_cached)
shows_hist = "Failure history" in failures and "ofl/oldfail" in failures
print("failures tab shows the persistent history (incl. an older failure):", shows_hist)
assert no_crash and shows_cached and shows_hist, full[-900:]
print("\nPTY-PERSIST-UI OK")
