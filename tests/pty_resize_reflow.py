"""Sections fill the available height (water-fill) instead of hard-capping at 2 items, and the
layout re-flows live when the terminal is resized (KEY_RESIZE / SIGWINCH). Regression for the
'lots of blank space while items are collapsed' report."""
import os, sys, pty, select, time, json, struct, fcntl, termios, signal

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_resize/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 5.0, "disk_used_delta": 0, "disk_build_total": 1 << 30, "disk_free": 1 << 30,
    "jobs": 6, "paused": False, "total": 50,
    "counts": {"built": 0, "failed": 42, "building": 0, "queued": 8, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 0}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "", "building": [],
    "failures_recent": [{"slug": f"ofl/fam{i}", "error": f"e{i}", "log": ""} for i in range(42)],
    "built_recent": [],
    "archive_recent": [{"status": "added", "repo": f"o/repo{i}"} for i in range(7)],
    "cohorts": [], "migration": {}, "op_stats": {}, "phase_durations": {},
    "tasks": [{"key": "build", "name": "build", "status": "running", "elapsed": 5,
               "done": 0, "total": 50, "detail": ""}],
    "control_log": [], "dep_relaxations": [], "config_path": "",
    "config": {"build_dir": BUILD, "source": "metadata", "google_fonts": "/gf", "archive": "/a",
               "backend": "auto", "fontc_bin": None, "jobs": 6, "percent": 3.0, "timeout": None,
               "populate_archive": False, "manage_venvs": False, "compare": False},
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
out = b""


def setsize(rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    try:
        os.kill(pid, signal.SIGWINCH)
    except OSError:
        pass


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


def shown(text):
    return sum(1 for i in range(42) if f"ofl/fam{i}" in text)


TAB = b"\t"
setsize(20, 140); drain(0.7)
drain(0.7)                          # overview (default), SMALL (20 rows)
out = b""; drain(0.6)
small = out.decode("utf-8", "replace")
small_n = shown(small)

out = b""; setsize(48, 140); drain(0.9)                # GROW to 48 rows
big = out.decode("utf-8", "replace")
big_n = shown(big)

os.write(fd, b"q"); drain(1.0)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)

no_crash = "_curses.error" not in (small + big) and "Traceback" not in (small + big)
print("no crash:", no_crash)
print(f"failures shown — small(20 rows): {small_n}   after grow(48 rows): {big_n}")
assert no_crash, (small + big)[-800:]
assert big_n > small_n and big_n > 10, f"grow did not add rows: {small_n} -> {big_n}"
print("\nPTY-RESIZE-REFLOW OK")
