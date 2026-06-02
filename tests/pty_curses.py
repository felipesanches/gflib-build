"""Render CursesFrontend (monitor mode) against a crafted status.json in a pty; send view
switches + arrow keys + q. Validates the task-list / emoji / archive panel / arrow tabs draw
without crashing and that q returns cleanly."""
import json, os, sys, time, pty, select

BUILD = "/tmp/_pty_curses/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 42.0, "disk_used_delta": 1 << 20, "disk_free": 1 << 30, "jobs": 4,
    "paused": False, "total": 5,
    "counts": {"built": 2, "failed": 1, "building": 1, "queued": 1, "skipped": 0},
    "backends": {"fontc": 1, "fontmake": 1}, "phase": "archive", "phase_total": 10,
    "phase_done": 4, "phase_label": "added: owner/repo4", "phase_error": "",
    "building": [{"slug": "ofl/fam3", "worker": 1, "dur": 5.0, "backend": "fontmake", "note": ""}],
    "failures_recent": [{"slug": "ofl/fam9", "error": "boom", "log": "logs/x.log"}],
    "cohorts": [{"key": "base", "count": 3, "requirements": ""}],
    "migration": {"fontc": 1, "fontmake_fallback": 1, "fontmake_only": 0,
                  "both_identical": 0, "both_differ": 0},
    "op_stats": {}, "phase_durations": {"clone_gf": 3.1, "archive": 8.0},
    "tasks": [
        {"key": "clone_gf", "name": "clone google/fonts", "status": "done",
         "elapsed": 3.1, "done": 0, "total": 0, "detail": "/tmp/gf"},
        {"key": "build_fontc", "name": "build fontc from source", "status": "skipped",
         "elapsed": 0, "done": 0, "total": 0, "detail": ""},
        {"key": "discover", "name": "discover worklist", "status": "done",
         "elapsed": 0.4, "done": 0, "total": 0, "detail": "5 queued"},
        {"key": "archive", "name": "populate archive", "status": "running",
         "elapsed": 8.0, "done": 4, "total": 10, "detail": "added: owner/repo4"},
        {"key": "cohorts", "name": "scan dependency cohorts", "status": "pending",
         "elapsed": 0, "done": 0, "total": 0, "detail": ""},
        {"key": "build", "name": "build fonts", "status": "pending",
         "elapsed": 0, "done": 0, "total": 0, "detail": ""},
    ],
    "archive_recent": [{"status": "added", "repo": f"owner/repo{i}"} for i in range(4)]
                      + [{"status": "failed", "repo": "owner/badrepo"}],
    "done": False,
}
open(os.path.join(BUILD, "status.json"), "w").write(json.dumps(status))

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
harness = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
ms = g.MonitorState("{BUILD}")
fe = g.CursesFrontend(ms)
fe.monitor = True
fe.run()
import sys; print("CURSES_OK", file=sys.stderr); sys.stderr.flush()
'''

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execvp(sys.executable, [sys.executable, "-c", harness])
else:
    out = b""

    def drain(seconds):                              # keep reading so the pty buffer never fills
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

    drain(1.5)
    DOWN, UP, RIGHT, LEFT, ENTER, ESC = b"\x1b[B", b"\x1b[A", b"\x1b[C", b"\x1b[D", b"\r", b"\x1b"
    seqs = [
        DOWN, DOWN,              # overview: move task selection
        ENTER, DOWN, DOWN, ESC,  # open a task detail, scroll it, close
        RIGHT,                   # -> cohorts
        DOWN, ENTER, ESC,        # select a cohort, open requirements detail, close
        RIGHT,                   # -> failures
        DOWN, ENTER, DOWN, ESC,  # select a failure, open log detail, scroll, close
        RIGHT,                   # -> stats
        DOWN, ENTER, ESC,        # select an op, open detail, close
        LEFT, LEFT,              # arrow tabs backwards
        b"1", b"2", b"3", b"4",  # number jumps
    ]
    for seq in seqs:
        os.write(fd, seq)
        drain(0.18)
    os.write(fd, b"q")                       # quit the monitor
    end = time.time() + 10                    # drain until the child exits (EOF) so we see CURSES_OK
    while time.time() < end:
        if not drain(0.4):                    # drain() returns False on EOF
            break
    _, st = os.waitpid(pid, 0)
    ok = b"CURSES_OK" in out
    print("exit status:", os.waitstatus_to_exitcode(st))
    print("CURSES_OK seen:", ok)
    sys.exit(0 if ok and os.waitstatus_to_exitcode(st) == 0 else 1)
