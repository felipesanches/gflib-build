"""End-to-end self-heal: a first build fails families with a FIXABLE cause (broken venv), then a
second build (what pressing [C] -> Start does) AUTO-RETRIES them — they are re-queued and built,
without the user setting any 'retry' flag. This is the exact scenario the user hit: re-running was
showing 'BUILD COMPLETE' instantly without retrying. Genuine build errors are NOT auto-retried."""
import os
import sys
import time
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

import shutil
shutil.rmtree("/tmp/_retry_flow", ignore_errors=True)
os.makedirs("/tmp/_retry_flow/build", exist_ok=True)

args = types.SimpleNamespace(
    build_dir="/tmp/_retry_flow/build", google_fonts="/tmp/_retry_flow/gf",
    archive="/tmp/_retry_flow/archive", source="metadata", archive_rev="HEAD",
    backend="fontmake", fontc_bin=None, jobs=3, percent=100.0, only="",
    populate_archive=False, manage_venvs=False, base_python="python3", base_requirements=None,
    build_python="python3", timeout=None, rebuild=False, retry_failed=False, compare=False,
    keep_work=False, keep_fonts=True, mirror_missing=False, _want_build_fontc=False,
    _data_dir="/tmp/_retry_flow")

FAMS = [g.Family(f"ofl/fam{i}", f"Fam {i}", f"https://github.com/o/r{i}", "abc", None, False, [])
        for i in range(5)]
g.ensure_google_fonts = lambda path, on_progress=None: (time.sleep(0.05) or path)
g.discover = lambda gf: (FAMS, 5, 0)


def run_to_done(orch, label):
    orch.run()
    for _ in range(400):
        if orch.snapshot().get("done"):
            break
        time.sleep(0.05)
    orch.join()
    orch.save_state()
    c = orch.snapshot()["counts"]
    print(f"{label}: {c}")
    return c


# ---- run 1: every family fails with a FIXABLE 'broken venv' error ----
def build_fail(self, wid, slug):
    with self.lock:
        r = self.results[slug]
        r.status, r.error = "failed", "fontmake: ... No module named 'gftools'"
        r.started = r.ended = time.time()
g.Orchestrator._build_one = build_fail
c1 = run_to_done(g.Orchestrator(args), "run 1 (all fail: broken venv)")
assert c1["failed"] == 5 and c1["built"] == 0, c1

# ---- run 2 = pressing [C] -> Start again. The venv 'cause' is fixable, so auto-retry kicks in
#      and (with the bug now fixed) the families build instead of instantly 'BUILD COMPLETE'. ----
def build_ok(self, wid, slug):
    with self.lock:
        r = self.results[slug]
        r.status, r.backend, r.started, r.ended = "built", "fontmake", time.time(), time.time()
g.Orchestrator._build_one = build_ok
c2 = run_to_done(g.Orchestrator(args), "run 2 (auto-retry)")
assert c2["built"] == 5, f"auto-retry should have rebuilt all 5 previously-failed families: {c2}"
assert c2["failed"] == 0, c2
print("auto-retry rebuilt the previously-failed families on the second run")

# ---- run 3: a GENUINE build error must NOT be auto-retried (would just re-fail) ----
def build_genuine_fail(self, wid, slug):
    with self.lock:
        r = self.results[slug]
        r.status, r.error = "failed", "gftools.builder exit 1: KeyError 'instances'"
        r.started = r.ended = time.time()
g.Orchestrator._build_one = build_genuine_fail
shutil.rmtree("/tmp/_retry_flow2", ignore_errors=True)
os.makedirs("/tmp/_retry_flow2/build", exist_ok=True)
args.build_dir = "/tmp/_retry_flow2/build"; args._data_dir = "/tmp/_retry_flow2"
args.archive = "/tmp/_retry_flow2/archive"; args.google_fonts = "/tmp/_retry_flow2/gf"
c3a = run_to_done(g.Orchestrator(args), "run A (genuine build errors)")
assert c3a["failed"] == 5, c3a
# re-run: genuine errors are NOT auto-retried → nothing to build, stays failed
g.Orchestrator._build_one = build_ok
c3b = run_to_done(g.Orchestrator(args), "run B (no auto-retry for genuine errors)")
assert c3b["built"] == 0 and c3b["failed"] == 5, f"genuine errors must not auto-retry: {c3b}"
print("genuine build errors are NOT auto-retried (no wasteful rebuilds)")

print("\nSELF-HEAL-RETRY-FLOW OK")
