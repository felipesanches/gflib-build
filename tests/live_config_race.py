"""Stress the live-config completion race (review C1/C2): many percent bumps interleaved with
the build draining must never orphan a queued family, and a bump AFTER the build truly finished
must be ignored (not falsely reported as applied)."""
import os
import sys
import threading
import time
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

N = 120
FAMS = [g.Family(f"ofl/f{i:03d}", "F", f"https://github.com/o/r{i:03d}", "c", None, False,
                 [f"F{i}.ttf"]) for i in range(N)]
g.ensure_google_fonts = lambda p, on_progress=None: p
g.discover = lambda gf: (FAMS, N, 0)
g.git_clone_mirror = lambda url, dest, timeout=1800, stop=None: (os.makedirs(dest, exist_ok=True) or (0, "", ""))
g.git = lambda argv, timeout=None, **kw: (0, "", "")


def fast_build(self, wid, slug):              # ~40ms builds: the worklist outlives the bump cadence
    time.sleep(0.04)
    with self.lock:
        r = self.results[slug]
        r.status, r.backend, r.started, r.ended = "built", "fontmake", time.time(), time.time()


g.Orchestrator._build_one = fast_build

args = types.SimpleNamespace(
    build_dir="/tmp/_live_race/build", google_fonts="/tmp/_live_race/gf", archive="/tmp/_live_race/a",
    source="metadata", archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=4,
    percent=10.0, only="", populate_archive=False, manage_venvs=False, base_python="python3",
    base_requirements=None, build_python="python3", timeout=None, rebuild=False,
    retry_failed=False, compare=False, keep_work=False, keep_fonts=True, mirror_missing=False,
    _want_build_fontc=False, _data_dir="/tmp/_live_race")
os.makedirs(args.build_dir, exist_ok=True)

orch = g.Orchestrator(args)
orch.run()

# hammer percent up in fine steps; small delays so worklists repeatedly drain mid-stream
# (each drain that coincides with the next bump exercises the C1 completion race)
for pct in (18, 28, 40, 55, 72, 88, 100):
    time.sleep(0.015)
    g.write_control(args.build_dir, {"percent": pct})

# wait to settle
for _ in range(800):
    s = orch.snapshot()
    if s["done"]:
        break
    time.sleep(0.02)
orch.join()
s = orch.snapshot()
c = s["counts"]
print(f"final: built={c['built']} failed={c['failed']} queued={c['queued']} "
      f"building={c['building']} total={s['total']} pct={s['config']['percent']}")
# 100% → all 200 built, nothing orphaned in the queue (no lost families across drain-races)
assert s["total"] == N, s["total"]
assert c["built"] == N, c
assert c["queued"] == 0 and c["building"] == 0, c

# C2: apply_live AFTER completion (stop set) must be IGNORED, not falsely reported as applied
assert orch.stop.is_set()
orch.apply_live({"percent": 100, "jobs": 99})
s2 = orch.snapshot()
print("post-done jobs:", s2["config"]["jobs"], "| last control_log:", (s2["control_log"] or ["-"])[-1])
assert s2["config"]["jobs"] == s["config"]["jobs"], s2["config"]          # jobs NOT changed to 99
assert any("ignored" in ln for ln in s2.get("control_log", [])), s2.get("control_log")
print("\nLIVE-RACE OK (no orphans across bumps; post-completion change ignored)")
