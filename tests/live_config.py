"""Live config: writing control.json while the build runs must take effect immediately —
bumping percent enqueues the newly-included families (they get fetched/cohorted/built), and
bumping jobs spawns more workers. No restart. Exercises the real control-watcher + apply_live."""
import os
import sys
import threading
import time
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# 100 families; start at 10% (10 built), then bump to 50% live -> ~50 built total
FAMS = [g.Family(f"ofl/f{i:03d}", "F", f"https://github.com/o/r{i:03d}", "c", None, False,
                 [f"F{i}.ttf"]) for i in range(100)]
g.ensure_google_fonts = lambda p, on_progress=None: p
g.discover = lambda gf: (FAMS, 100, 0)
g.git_clone_mirror = lambda url, dest, timeout=1800, stop=None: (os.makedirs(dest, exist_ok=True) or (0, "", ""))
g.git = lambda argv, timeout=None, **kw: (0, "", "")

build_lock = threading.Lock()
worker_ids = set()


def slow_build(self, wid, slug):              # ~0.1s builds; record which workers ran
    with build_lock:
        worker_ids.add(wid)
    time.sleep(0.1)
    with self.lock:
        r = self.results[slug]
        r.status, r.backend, r.started, r.ended = "built", "fontmake", time.time(), time.time()


g.Orchestrator._build_one = slow_build

args = types.SimpleNamespace(
    build_dir="/tmp/_live_cfg/build", google_fonts="/tmp/_live_cfg/gf", archive="/tmp/_live_cfg/a",
    source="metadata", archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=2,
    percent=10.0, only="", populate_archive=False, manage_venvs=False, base_python="python3",
    base_requirements=None, build_python="python3", timeout=None, rebuild=False,
    retry_failed=False, compare=False, keep_work=False, keep_fonts=True, mirror_missing=False,
    _want_build_fontc=False, _data_dir="/tmp/_live_cfg")
os.makedirs(args.build_dir, exist_ok=True)

orch = g.Orchestrator(args)
orch.run()
# wait until the build is underway (some built at 10%)
for _ in range(200):
    if orch.snapshot()["counts"]["built"] >= 3:
        break
    time.sleep(0.02)
mid = orch.snapshot()
print(f"mid (10%, jobs=2): built={mid['counts']['built']}  total={mid['total']}  "
      f"config.percent={mid['config']['percent']}")

# LIVE change: 10% -> 50% and jobs 2 -> 6, via control.json (as the monitor would)
g.write_control(args.build_dir, {"percent": 50, "jobs": 6})

# wait for it to take effect + finish
for _ in range(400):
    s = orch.snapshot()
    if s["done"]:
        break
    time.sleep(0.05)
orch.join()
s = orch.snapshot()
print(f"after live bump: built={s['counts']['built']}  total={s['total']}  "
      f"config.percent={s['config']['percent']}  config.jobs={s['config']['jobs']}")
print("workers that ran:", sorted(worker_ids))
print("control_log:", s.get("control_log"))
# 50% of 100 = 50 families queued & built (up from 10); jobs grew to 6 (more worker ids)
assert s["total"] == 50, s["total"]
assert s["counts"]["built"] == 50, s["counts"]
assert s["config"]["percent"] == 50.0, s["config"]
assert s["config"]["jobs"] == 6, s["config"]
assert len(worker_ids) >= 5, f"jobs bump should have spawned more workers: {worker_ids}"
assert any("percent" in ln for ln in s.get("control_log", [])), s.get("control_log")
print("\nLIVE-CONFIG OK (percent bump enqueued+built more; jobs bump spawned workers; live)")
