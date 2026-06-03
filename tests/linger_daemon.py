"""The detached daemon LINGERS after the build completes (instead of exiting), so a live [R] retry
or % bump is applied in the SAME process — no re-exec, the failures/cohorts lists stay intact. When
no more work arrives within LINGER_SECONDS, the daemon idle-exits. (Foreground/headless runs, where
_linger is False, still complete and exit immediately — covered by smoke_tasklist etc.)"""
import os
import sys
import time
import types
import shutil

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

shutil.rmtree("/tmp/_linger", ignore_errors=True)
os.makedirs("/tmp/_linger/build", exist_ok=True)
args = types.SimpleNamespace(
    build_dir="/tmp/_linger/build", google_fonts="/tmp/_linger/gf", archive="/tmp/_linger/a",
    source="metadata", archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=2, percent=100.0,
    only="", populate_archive=False, manage_venvs=False, base_python="python3", base_requirements=None,
    build_python="python3", timeout=None, rebuild=False, retry_failed=False, compare=False,
    keep_work=False, keep_fonts=True, mirror_missing=False, _want_build_fontc=False,
    _data_dir="/tmp/_linger")

FAMS = [g.Family(f"ofl/fam{i}", f"Fam {i}", "u", "c", None, False, []) for i in range(3)]
g.ensure_google_fonts = lambda p, on_progress=None: p
g.discover = lambda gf: (FAMS, 3, 0)

mode = {"v": "fail"}                                   # first all fail, then succeed on retry


def build_one(self, wid, slug):
    with self.lock:
        r = self.results[slug]
        if mode["v"] == "fail":
            r.status, r.error = "failed", "venv: No module named 'gftools'"
        else:
            r.status, r.backend = "built", "fontmake"
        r.started = r.ended = time.time()
g.Orchestrator._build_one = build_one

o = g.Orchestrator(args)
o._linger = True
o.LINGER_SECONDS = 5
o.run()

# 1) build completes (all fail) and the daemon LINGERS: done, but not stopped
for _ in range(200):
    if o.snapshot()["done"] and not o.stop.is_set():
        break
    time.sleep(0.05)
assert o.snapshot()["done"] and not o.stop.is_set(), "should linger (done, not stopped)"
assert o.snapshot()["counts"]["failed"] == 3, o.snapshot()["counts"]
print("after completion the daemon lingers: phase=done, not stopped, 3 failed")

# 2) a live retry (what [R] sends) resumes the SAME process and builds it — no re-exec
mode["v"] = "ok"
o.apply_live({"retry": ["ofl/fam0"]})
for _ in range(200):
    if o.results["ofl/fam0"].status == "built":
        break
    time.sleep(0.05)
assert o.results["ofl/fam0"].status == "built", "live retry should resume + build in-process"
assert not o.stop.is_set(), "daemon should still be lingering after the retry"
print("live retry resumed the lingering daemon and built the family (no re-exec)")

# 3) with no further work, it idle-exits after LINGER_SECONDS
for _ in range(300):
    if o.stop.is_set():
        break
    time.sleep(0.05)
assert o.stop.is_set(), "daemon should idle-exit after LINGER_SECONDS"
o.join()
print("daemon idle-exited after the linger timeout")

print("\nLINGER-DAEMON OK")
