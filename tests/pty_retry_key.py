"""Pressing [R] on a selected failed family during a LIVE build writes a {"retry": [slug]} control
message (which the daemon picks up to re-queue it). Exercises the key binding + control.json write
end-to-end; the daemon-side apply_live/_requeue is covered by retry_action.py."""
import json, os, sys, time, pty, select

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_retry/build"
os.makedirs(BUILD, exist_ok=True)
for f in ("control.json",):
    try:
        os.unlink(os.path.join(BUILD, f))
    except OSError:
        pass
status = {
    "elapsed": 30.0, "disk_used_delta": 0, "disk_build_total": 1 << 30, "disk_free": 1 << 30,
    "jobs": 4, "paused": False, "total": 6,
    "counts": {"built": 1, "failed": 2, "building": 1, "queued": 2, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 1}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "",
    "building": [{"slug": "ofl/now", "worker": 1, "dur": 5.0, "backend": "fontmake", "note": ""}],
    "failures_recent": [{"slug": "ofl/brokenvenv", "error": "venv: No module named 'gftools'", "log": ""},
                        {"slug": "ofl/other", "error": "boom", "log": ""}],
    "built_recent": [], "fail_categories": [],          # no 'by cause' section → failures is section 0
    "archive_recent": [], "cohorts": [], "migration": {}, "op_stats": {}, "phase_durations": {},
    "tasks": [{"key": "build", "name": "build", "status": "running", "elapsed": 30,
               "done": 3, "total": 6, "detail": ""}],
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
# make the monitor believe a live daemon is running: this child's cmdline contains "gflib_build",
# so read_daemon_pid(build_dir) treats it as alive
open(BUILD + "/daemon.pid", "w").write(str(pid))
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


TAB = b"\t"
drain(0.9)
for _ in range(3):                                        # overview -> cohorts -> built -> failures
    os.write(fd, TAB); drain(0.25)
drain(0.3)
os.write(fd, b"R"); drain(0.6)                            # retry the selected (first) failure
txt = out.decode("utf-8", "replace")
os.write(fd, b"q"); drain(0.8)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)

no_crash = "_curses.error" not in txt and "Traceback" not in txt
print("no crash:", no_crash)
foot_hint = "[R]etry" in txt
print("footer advertises [R]etry:", foot_hint)
ctl = {}
try:
    ctl = json.loads(open(BUILD + "/control.json").read())
except Exception as e:
    print("no control.json:", e)
retry = (ctl.get("set") or {}).get("retry") or ctl.get("retry")
print("control.json retry list:", retry)
assert no_crash and foot_hint, txt[-700:]
assert retry == ["ofl/brokenvenv"], f"[R] should have requested retry of the selected failure: {ctl}"
print("\nPTY-RETRY-KEY OK")
