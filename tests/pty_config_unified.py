"""The unified Configuration tab: setup mode (SetupState) renders the full editable form,
↓ navigates fields + Start/Cancel, ▶ Start returns the typed config; q cancels. Live mode
(MonitorState) renders + edits live fields without crashing."""
import json, os, sys, time, pty, select

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run_pty(harness, keys, settle=8):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm"
        os.execvp(sys.executable, [sys.executable, "-c", harness])
    out = b""

    def drain(sec):
        nonlocal out
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

    drain(1.0)
    for k in keys:
        os.write(fd, k)
        drain(0.2)
    end = time.time() + settle
    while time.time() < end:
        if not drain(0.3):
            break
    if out[-0:] is not None:                          # ensure the child exits even if a key path
        try:                                          # didn't (so waitpid never hangs the test)
            os.write(fd, b"q")
        except OSError:
            pass
    drain(2)
    try:
        os.waitpid(pid, os.WNOHANG)
        os.kill(pid, 9)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except OSError:
        pass
    return out.decode("utf-8", "replace")


# ---- setup mode: render + navigate to ▶ Start + Enter -> returns config ----
SETUP = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
cfg = {{"source":"metadata","google_fonts":"/gf","archive":"/a","build_dir":"/b","backend":"auto",
       "fontc_bin":"","jobs":8,"percent":100.0,"timeout":None,"populate_archive":True,
       "manage_venvs":False,"compare":True}}
fe = g.CursesFrontend(g.SetupState(cfg, "/b", "/cfg")); fe.setup = True
r = fe.run()
print("RESULT=" + repr(r), file=sys.stderr); sys.stderr.flush()
'''
UP = b"\x1bOA"          # application-keypad up-arrow; "▶ Start build" is always 2nd-from-last
# (before "Cancel"), so UP x2 from the first field reaches it regardless of how many fields show
txt = run_pty(SETUP, [UP, UP, b"\r"], settle=3)
assert "Configuration" in txt, txt[-400:]
assert "Start build" in txt and "google/fonts" in txt and "percent of library" in txt, txt[-600:]
print("setup renders the full editable Configuration form")
assert "RESULT={" in txt, "arrow navigation to ▶ Start failed:\n" + txt[-500:]
assert "'source': 'metadata'" in txt and "'jobs': 8" in txt, txt[-400:]
print("↑-navigation reaches ▶ Start, which returns the typed config")

# ---- q cancels setup -> None ----
txt2 = run_pty(SETUP, [b"q"], settle=2)
assert "RESULT=None" in txt2, txt2[-300:]
print("q cancels setup -> None")

# ---- live mode renders without crashing ----
BUILD = "/tmp/_pty_cfgu/build"; os.makedirs(BUILD, exist_ok=True)
status = {"elapsed": 5.0, "disk_used_delta": 0, "disk_free": 1 << 30, "jobs": 4, "paused": False,
          "total": 5, "counts": {"built": 1, "failed": 0, "building": 1, "queued": 3, "skipped": 0},
          "backends": {"fontc": 0, "fontmake": 1}, "phase": "build", "phase_total": 0,
          "phase_done": 0, "phase_label": "", "phase_error": "", "building": [], "failures_recent": [],
          "cohorts": [], "migration": {}, "op_stats": {}, "phase_durations": {}, "tasks": [],
          "archive_recent": [], "control_log": [], "dep_relaxations": [], "config_path": "",
          "config": {"source": "metadata", "google_fonts": "/gf", "archive": "/a", "build_dir": BUILD,
                     "backend": "auto", "fontc_bin": None, "jobs": 4, "percent": 10.0, "timeout": None,
                     "populate_archive": True, "manage_venvs": True, "compare": False}, "done": False}
open(BUILD + "/status.json", "w").write(json.dumps(status))
LIVE = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
fe = g.CursesFrontend(g.MonitorState("{BUILD}")); fe.monitor = True
fe.run()
print("LIVE_OK", file=sys.stderr); sys.stderr.flush()
'''
txt3 = run_pty(LIVE, [b"\t", b"\t", b"\t"], settle=2)   # tab around (q added by run_pty)
assert "LIVE_OK" in txt3 and "_curses.error" not in txt3 and "Traceback" not in txt3, txt3[-400:]
print("live config tab + tabs render without crashing")
print("\nPTY-CONFIG-UNIFIED OK")
