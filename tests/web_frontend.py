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

# --- security hardening (adversarial-review fixes) ---
# 1) the page carries NO inline onclick — clicks go through delegated data-* handlers, so a slug
#    can never break out into executable code (and E() escapes ' as well).
assert "onclick=" not in base, "no inline onclick (event delegation only)"
assert "data-slug" in base and "data-act" in base
print("no inline onclick; clicks via data-* delegation (no JS injection via slugs)")

# 2) apply_live CLAMPS jobs to MAX_JOBS and percent to [0,100] — control.json is untrusted, a bad
#    value must never spawn a thread-bomb or an absurd worklist.
import types
oa = types.SimpleNamespace(
    only="", rebuild=False, retry_failed=False, retry_category="", build_dir=str(build_dir),
    google_fonts=None, archive=str(build_dir / "a"), source="archive", backend="fontmake", fontc_bin=None,
    jobs=4, percent=10.0, populate_archive=False, manage_venvs=False, base_python="python3",
    base_requirements=None, build_python="python3", timeout=None, compare=False, keep_work=False,
    keep_fonts=True, mirror_missing=False, _want_build_fontc=False, _data_dir=str(build_dir),
    archive_rev="HEAD")
os.makedirs(str(build_dir / "a"), exist_ok=True)
orch = g.Orchestrator(oa)
orch._all_families = []
seen = []
orch._ensure_workers = lambda n: seen.append(n)
orch.apply_live({"jobs": 99999})
assert seen and seen[-1] == g.MAX_JOBS, ("jobs must clamp to MAX_JOBS", seen)
orch.apply_live({"percent": 99999})
assert orch.args.percent == 100.0, ("percent must clamp to 100", orch.args.percent)
print("apply_live clamps jobs->MAX_JOBS(%d) and percent->[0,100]" % g.MAX_JOBS)

# 3) a negative Content-Length must not hang a handler thread (bounded body read).
c = socket.create_connection(("127.0.0.1", port), timeout=5)
c.sendall(b"POST /api/control HTTP/1.1\r\nHost: x\r\nContent-Length: -1\r\nConnection: close\r\n\r\n{}")
c.settimeout(6)
resp = b""
try:
    while True:
        d = c.recv(4096)
        if not d:
            break
        resp += d
except socket.timeout:
    pass
c.close()
assert b"200" in resp and b"ok" in resp, ("server must respond to negative Content-Length, not hang", resp[:80])
print("negative Content-Length -> quick response, no hung thread")

print("\nWEB-FRONTEND OK")
