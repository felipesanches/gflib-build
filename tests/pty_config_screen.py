"""The first-run Configuration screen (config_screen, formerly the 'wizard') renders with the
dashboard tab-bar chrome, edits fields, and ▶ Start returns the typed settings (Esc cancels)."""
import os, sys, time, pty, select, struct, fcntl, termios

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

harness = f'''
import sys; sys.path.insert(0, "{REPO}")
import gflib_build as g
spec = [
    {{"key": "source", "label": "worklist source", "type": "choice",
      "value": "metadata", "choices": ["metadata", "archive"]}},
    {{"key": "backend", "label": "build backend", "type": "choice",
      "value": "auto", "choices": ["auto", "fontc", "fontmake", "both"]}},
    {{"key": "percent", "label": "percent of library", "type": "stepnum",
      "value": "100", "step": 5, "min": 1, "max": 100}},
    {{"key": "manage_venvs", "label": "cohort venvs", "type": "bool", "value": True}},
]
res = g.config_screen(spec, lambda v: [f"backend={{v['backend']}}  percent={{v['percent']}}"])
print("RESULT=" + repr(res), file=sys.stderr); sys.stderr.flush()
'''

def run(keys, cols=None):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm"
        os.execvp(sys.executable, [sys.executable, "-c", harness])
    if cols:
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, cols, 0, 0))
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
        os.write(fd, k); drain(0.2)
    end = time.time() + 8
    while time.time() < end:
        if not drain(0.4):
            break
    os.waitpid(pid, 0)
    return out.decode("utf-8", "replace")

# Tab through the 4 fields to the "▶ Start build" button, then Enter -> returns typed settings.
# (Arrow keys split unreliably under a pty getch as a bare ESC=cancel, so we don't use them here.)
txt = run([b"\t", b"\t", b"\t", b"\t", b"\r"])
print("start path RESULT present:", "RESULT={" in txt)
assert "RESULT={" in txt, txt
assert "'backend': 'auto'" in txt and "'percent': 100.0" in txt, txt
print("start returned the typed settings")

# Esc cancels -> None
txt2 = run([b"\x1b"])
assert "RESULT=None" in txt2, txt2
print("esc cancels -> None")

# narrow terminal (exactly 20 cols, where "▶ Start build" pushes "Cancel" to n=0) must NOT crash
txt3 = run([b"\x1b"], cols=20)
assert "_curses.error" not in txt3 and "Traceback" not in txt3, txt3[-300:]
assert "RESULT=None" in txt3, txt3[-300:]
print("width-20 renders without crashing")
print("\nPTY-CONFIG-SCREEN OK")
