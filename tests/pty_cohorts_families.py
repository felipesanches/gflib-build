"""The cohorts tab ('Dependency cohorts' section) lists, on each row, a comma-separated list of
the family NAMES assigned to that cohort (not just the requirements snippet)."""
import json, os, sys, time, pty, select

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = "/tmp/_pty_coh/build"
os.makedirs(BUILD, exist_ok=True)
status = {
    "elapsed": 5.0, "disk_used_delta": 0, "disk_build_total": 1 << 30, "disk_free": 1 << 30,
    "jobs": 4, "paused": False, "total": 3,
    "counts": {"built": 0, "failed": 0, "building": 0, "queued": 3, "skipped": 0},
    "backends": {"fontc": 0, "fontmake": 0}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "", "building": [], "failures_recent": [], "built_recent": [],
    "archive_recent": [],
    "cohorts": [{"key": "base", "count": 2, "requirements": "", "families": ["Oswald", "Roboto"]},
                {"key": "cohort_ab", "count": 1, "requirements": "fontmake==1.0",
                 "families": ["Lato"]}],
    "migration": {}, "op_stats": {}, "phase_durations": {}, "tasks": [], "control_log": [],
    "dep_relaxations": [], "config_path": "",
    "config": {"build_dir": BUILD, "source": "archive", "google_fonts": None, "archive": "/a",
               "backend": "auto", "fontc_bin": None, "jobs": 4, "percent": 100.0, "timeout": None,
               "populate_archive": False, "manage_venvs": True, "compare": False},
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
drain(0.8)
os.write(fd, TAB); os.write(fd, TAB); drain(0.6)      # overview -> queue -> cohorts
os.write(fd, b"\r"); drain(0.4)                        # open the focused cohort's detail overlay
txt = out.decode("utf-8", "replace")
os.write(fd, b"q"); drain(1.0)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)

no_crash = "_curses.error" not in txt and "Traceback" not in txt
print("no crash:", no_crash)
row = "Oswald | Roboto" in txt                         # base cohort row lists both names ( | sep)
small = "Lato" in txt                                  # the other cohort's single family
detail = "family names:" in txt                        # detail overlay has a family-names block
print("base row shows 'Oswald | Roboto':", row)
print("cohort_ab row shows 'Lato':", small)
print("detail overlay lists family names:", detail)
assert no_crash and row and small and detail, txt[-800:]
print("\nPTY-COHORTS-FAMILIES OK")
