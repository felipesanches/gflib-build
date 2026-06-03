"""The web dashboard (--ui web): serves the HTML page at /, the live snapshot at /api/status, and
routes controls to control.json via POST /api/control — the same channel the curses monitor uses."""
import os
import sys
import json
import time
import socket
import threading
import urllib.request
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

build_dir = Path(tempfile.mkdtemp(prefix="_web_"))
status = {
    "elapsed": 42.0, "disk_build_total": 1 << 30, "disk_free": 2 << 30, "jobs": 4, "paused": False,
    "total": 10, "counts": {"built": 3, "failed": 2, "building": 1, "queued": 4, "skipped": 0},
    "backends": {"fontc": 2, "fontmake": 1}, "phase": "build", "phase_total": 0, "phase_done": 0,
    "phase_label": "", "phase_error": "",
    "building": [{"slug": "ofl/now", "worker": 1, "dur": 5.0, "backend": "fontmake", "note": ""}],
    "queued_list": [{"slug": "ofl/q1", "kind": "new"}, {"slug": "ofl/q2", "kind": "retry"},
                    {"slug": "ofl/q3", "kind": "rebuild"}],
    "failures_recent": [{"slug": "ofl/boom", "error": "kaboom", "log": ""}],
    "built_recent": [{"slug": "ofl/ok", "backend": "fontc", "bytes": 1000, "compare": "identical"}],
    "fail_categories": [{"cat": "build error", "count": 2, "hint": "open the log", "families": ["ofl/boom"]}],
    "cohorts": [{"key": "base", "count": 3, "requirements": "", "families": ["A", "B"]}],
    "op_stats": {}, "phase_durations": {}, "migration": {}, "tasks": [], "archive_recent": [],
    "control_log": [], "dep_relaxations": [], "config_path": "", "done": False,
    "config": {"jobs": 4, "percent": 10.0, "build_dir": str(build_dir), "source": "metadata"},
}
(build_dir / "status.json").write_text(json.dumps(status))

# pick a free port
s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()

fe = g.WebFrontend(g.MonitorState(build_dir))
fe.port = port
threading.Thread(target=fe.run, daemon=True).start()


def get(path):
    return urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=5).read().decode("utf-8")


# wait for the server to come up
base = None
for _ in range(50):
    try:
        base = get("/"); break
    except Exception:
        time.sleep(0.1)
assert base and "gflib-build dashboard" in base, "index HTML must serve"
assert "queue" in base and "Now building" in base, "page references the queue tab + pinned building"
print("GET / serves the dashboard HTML")

st = json.loads(get("/api/status"))
assert st["counts"]["failed"] == 2 and st["counts"]["queued"] == 4, st["counts"]
assert [q["kind"] for q in st["queued_list"]] == ["new", "retry", "rebuild"], st["queued_list"]
print("GET /api/status returns the live snapshot (queue kinds intact)")

req = urllib.request.Request(f"http://127.0.0.1:{port}/api/control",
                             data=json.dumps({"set": {"retry": ["ofl/boom"], "jobs": 6}}).encode(),
                             headers={"Content-Type": "application/json"}, method="POST")
resp = json.loads(urllib.request.urlopen(req, timeout=5).read())
assert resp["ok"] is True, resp
ctl = json.loads((build_dir / "control.json").read_text())
assert ctl["set"]["retry"] == ["ofl/boom"] and ctl["set"]["jobs"] == 6, ctl
print("POST /api/control writes control.json (retry + jobs) — same channel as the TUI")

# a bad path 404s without crashing the server
try:
    get("/nope")
    assert False, "should 404"
except urllib.error.HTTPError as e:
    assert e.code == 404
# server still alive
assert "dashboard" in get("/")
print("unknown path -> 404, server stays up")

print("\nWEB-FRONTEND OK")
