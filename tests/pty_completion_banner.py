"""When a build has finished (or the daemon died), the dashboard shows a prominent banner with the
outcome + the top failure cause + next steps — instead of looking frozen mid-flight. The failures
tab also shows a 'Failures by cause' summary. Covers UX features 1 (banner) and 3 (cause summary)."""
import json, os, sys, time, pty, select

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_banner/build"
os.makedirs(BUILD, exist_ok=True)
# done build, NO daemon.pid file (so daemon_alive == False), with a failure-cause breakdown
status = {
    "elapsed": 3600.0, "disk_used_delta": 0, "disk_build_total": 1 << 30, "disk_free": 1 << 30,
    "jobs": 6, "paused": False, "total": 106,
    "counts": {"built": 2, "failed": 83, "building": 0, "queued": 0, "skipped": 21},
    "backends": {"fontc": 2, "fontmake": 0}, "phase": "done", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "", "building": [],
    "failures_recent": [{"slug": "ofl/42dotsans", "error": "No module named 'gftools'", "log": ""}],
    "built_recent": [],
    "fail_categories": [
        {"cat": "broken dependency venv", "count": 52,
         "hint": "the cohort venv is missing its packages — it is rebuilt automatically on the next run"},
        {"cat": "transient fetch error", "count": 7, "hint": "a network hiccup — retried automatically"},
        {"cat": "repo not mirrored", "count": 6, "hint": "turn on 'populate archive'"}],
    "archive_recent": [], "cohorts": [], "migration": {}, "op_stats": {}, "phase_durations": {},
    "tasks": [{"key": "build", "name": "build fonts", "status": "done", "elapsed": 3600,
               "done": 85, "total": 106, "detail": ""}],
    "control_log": [], "dep_relaxations": [], "config_path": "",
    "config": {"build_dir": BUILD, "source": "metadata", "google_fonts": "/gf", "archive": "/a",
               "backend": "auto", "fontc_bin": None, "jobs": 6, "percent": 3.0, "timeout": None,
               "populate_archive": True, "manage_venvs": True, "compare": False},
    "done": True,
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
banner_txt = out.decode("utf-8", "replace")               # banner is visible on the landing tab
# tab to the failures view for the "Failures by cause" section
out = b""
for _ in range(4):                                         # config -> overview -> cohorts -> built -> failures
    os.write(fd, TAB); drain(0.25)
drain(0.4)
fail_txt = out.decode("utf-8", "replace")
os.write(fd, b"q"); drain(1.0)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)

full = banner_txt + fail_txt
no_crash = "_curses.error" not in full and "Traceback" not in full
print("no crash:", no_crash)
has_banner = "BUILD COMPLETE" in banner_txt
has_outcome = "2 built" in banner_txt and "83 failed" in banner_txt and "21 skipped" in banner_txt
has_cause = "broken dependency venv" in banner_txt
has_next = "reconfigure" in banner_txt or "re-run" in banner_txt
print("banner shows 'BUILD COMPLETE':", has_banner)
print("banner shows the outcome (built/failed/skipped):", has_outcome)
print("banner shows the top failure cause:", has_cause)
print("banner shows next steps:", has_next)
has_catsec = "Failures by cause" in fail_txt
has_catrow = "broken dependency venv" in fail_txt and "52" in fail_txt
print("failures tab has a 'Failures by cause' section:", has_catsec)
print("category row shows count + cause:", has_catrow)
assert no_crash and has_banner and has_outcome and has_cause and has_next and has_catsec and has_catrow, full[-1000:]
print("\nPTY-COMPLETION-BANNER OK")
