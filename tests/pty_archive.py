"""The Archive tab: shows the WHOLE-archive total (repos on disk), and a queue-oriented, colour-coded
multi-column view — cloning now (yellow), recently archived (green), queued next (cyan) — plus an
'unreachable' list with the git reason. (Replaces the old session-count 'Archive — mirrored' list.)"""
import json, os, sys, time, pty, select, struct, fcntl, termios, signal

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_arch/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 60.0, "disk_build_total": 1 << 30, "disk_free": 1 << 30, "jobs": 8, "paused": False,
    "total": 30, "counts": {"built": 2, "failed": 1, "building": 1, "queued": 5, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 0}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "", "building": [], "queued_list": [], "failures_recent": [],
    "built_recent": [], "fail_categories": [], "cohorts": [],
    "archive": {
        "total": 1302,
        "active": ["googlefonts/cloningnow", "ektype/cloning2"],
        "recent": [{"repo": "owner/fresh1", "status": "added", "ts": 1700000300.0, "reason": ""},
                   {"repo": "owner/fresh2", "status": "added", "ts": 1700000200.0, "reason": ""},
                   {"repo": "owner/deadrepo", "status": "failed", "ts": 1700000100.0,
                    "reason": "remote: Repository not found (HTTP 404)"}],
        "pending": [f"owner/queued{i}" for i in range(40)], "pending_total": 220,
    },
    "op_stats": {}, "phase_durations": {}, "migration": {},
    "tasks": [{"key": "archive", "name": "populate archive (mirror missing)", "status": "running",
               "elapsed": 100, "done": 363, "total": 1037, "detail": "present: evilmartians/mono"}],
    "archive_recent": [],
    "control_log": [], "dep_relaxations": [], "config_path": "", "done": False,
    "config": {"jobs": 8, "percent": 100.0, "build_dir": BUILD, "source": "metadata"},
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
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 44, 140, 0, 0))
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


TAB = b"\t"
drain(0.9)
os.write(fd, TAB); os.write(fd, TAB); os.write(fd, TAB); drain(0.5)   # overview->queue->cohorts->archive
for rows in (45, 44):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 140, 0, 0))
    try:
        os.kill(pid, signal.SIGWINCH)
    except OSError:
        pass
    drain(0.3)
txt = out.decode("utf-8", "replace")

# OLD-DAEMON case: status.json with the running archive task but NO new 'archive' dict (a daemon that
# predates the redesign) -> the tab must say 'mirroring in progress', NOT 'idle'.
status2 = dict(status); status2.pop("archive")
open(BUILD + "/status.json", "w").write(json.dumps(status2))
out = b""
for rows in (43, 44):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 140, 0, 0))
    try:
        os.kill(pid, signal.SIGWINCH)
    except OSError:
        pass
    drain(0.4)
txt2 = out.decode("utf-8", "replace")

os.write(fd, b"q"); drain(0.6)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)

no_crash = "_curses.error" not in txt and "Traceback" not in txt
print("no crash:", no_crash)
total = "1302 repos mirrored on disk" in txt                 # WHOLE archive, not session count
counts = "2 cloning now" in txt and "220 queued" in txt and "1 unreachable" in txt
cloning = "googlefonts/cloningnow" in txt
recent = "owner/fresh1" in txt
queued = "owner/queued0" in txt and "owner/queued5" in txt    # several queued repos visible (columns)
unreachable = "owner/deadrepo" in txt and ("404" in txt or "Repository not found" in txt)
mirroring = "363/1037" in txt and "mirroring" in txt          # the live archive-task progress
print("archive-task progress line (363/1037):", mirroring)
# multi-column: many distinct queued slugs visible at once
ncols = sum(1 for i in range(40) if f"owner/queued{i}" in txt)
print("total (whole archive on disk):", total)
print("counts line (cloning/queued/unreachable):", counts)
print("cloning-now repo shown:", cloning, "| recently-archived shown:", recent)
print("queued repos shown:", queued, "| unreachable + reason:", unreachable)
print("distinct queued slugs visible (multi-column):", ncols)
no_idle = "archive idle" not in txt2 and ("mirroring in progress" in txt2 or "363/1037" in txt2)
print("old daemon (no 'archive' dict) but task running -> NOT shown as idle:", no_idle)
assert no_crash and total and counts and cloning and recent and queued and unreachable, txt[-1000:]
assert ncols >= 12, f"expected a multi-column grid, saw {ncols} queued slugs"
assert mirroring, "the running archive task's progress must be shown"
assert no_idle, "a running archive task must never be reported as 'idle'"
print("\nPTY-ARCHIVE OK")
