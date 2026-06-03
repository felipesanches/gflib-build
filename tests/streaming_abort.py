"""The concurrent archive pre-warmer must not block completion: when the builds finish (which
sets `stop`), in-flight clones abort promptly and the driver returns quickly — it does NOT wait
for a slow clone to run to its 1800s timeout. Uses a slow, stop-aware fake clone."""
import os
import sys
import threading
import time
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

ARCHIVE = "/tmp/_stream_abort/archive"
os.makedirs(ARCHIVE, exist_ok=True)


def slow_clone(url, dest, timeout=1800, stop=None):
    # a "big" clone: ~5s, but abortable via stop (like the real git_clone_mirror)
    for _ in range(100):
        if stop is not None and stop.is_set():
            return (1, "", "aborted")
        time.sleep(0.05)
    os.makedirs(dest, exist_ok=True)
    return (0, "", "")


g.git_clone_mirror = slow_clone
g.git = lambda argv, timeout=None, **kw: (0, "", "")

FAMS = [g.Family(f"ofl/f{i}", "F", f"https://github.com/o/r{i}", "c", None, False, [f"F{i}.ttf"])
        for i in range(6)]
g.ensure_google_fonts = lambda p, on_progress=None: p
g.discover = lambda gf: (FAMS, 9, 3)


def fast_build(self, wid, slug):              # builds finish quickly (don't wait on mirrors)
    with self.lock:
        r = self.results[slug]
        r.status, r.backend, r.started, r.ended = "built", "fontmake", time.time(), time.time()


g.Orchestrator._build_one = fast_build

args = types.SimpleNamespace(
    build_dir="/tmp/_stream_abort/build", google_fonts="/tmp/_stream_abort/gf", archive=ARCHIVE,
    source="metadata", archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=4,
    percent=100.0, only="", populate_archive=True, manage_venvs=False, base_python="python3",
    base_requirements=None, build_python="python3", timeout=None, rebuild=False,
    retry_failed=False, compare=False, keep_work=False, keep_fonts=True, mirror_missing=False,
    _want_build_fontc=False, _data_dir="/tmp/_stream_abort")
os.makedirs(args.build_dir, exist_ok=True)

t0 = time.time()
orch = g.Orchestrator(args)
orch.run()
for _ in range(400):
    if orch.snapshot()["done"]:
        break
    time.sleep(0.05)
orch.join()
elapsed = time.time() - t0
s = orch.snapshot()
print(f"done in {elapsed:.1f}s  built={s['counts']['built']}  "
      f"archive_task={[t['status'] for t in s['tasks'] if t['key']=='archive']}")
# builds are instant; without the abort, the pre-warmer's slow clones would keep the driver
# pinned via prewarm.join. With stop honored mid-clone, the whole thing wraps up fast.
assert s["counts"]["built"] == 6, s["counts"]
assert elapsed < 8, f"driver blocked on the slow pre-warmer ({elapsed:.1f}s)"
# the final snapshot is coherent (archive task reached a terminal state, not stuck 'running')
assert [t["status"] for t in s["tasks"] if t["key"] == "archive"][0] in ("done", "failed")
print("STREAMING-ABORT OK (driver returns promptly; pre-warmer aborts on build completion)")
