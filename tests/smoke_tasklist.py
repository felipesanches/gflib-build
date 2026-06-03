"""Headless validation of the task-list pipeline: monkeypatch the heavy ops and verify the
driver walks clone_gf → build_fontc → discover → archive → cohorts → build, that snapshot()
exposes a coherent task-list, and that archive_recent grows live. No real clones/builds."""
import os
import sys
import time
import types
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# ---- fake args (only the fields the orchestrator/driver touch) ----
args = types.SimpleNamespace(
    build_dir="/tmp/_smoke_tl/build", google_fonts="/tmp/_smoke_tl/gf",
    archive="/tmp/_smoke_tl/archive", source="metadata", archive_rev="HEAD",
    backend="fontmake", fontc_bin=None, jobs=3, percent=100.0, only="",
    populate_archive=True, manage_venvs=False, base_python="python3", base_requirements=None,
    build_python="python3", timeout=None, rebuild=False, retry_failed=False,
    compare=False, keep_work=False, keep_fonts=True, mirror_missing=False,
    _want_build_fontc=True, _data_dir="/tmp/_smoke_tl",
)
import os, shutil
shutil.rmtree("/tmp/_smoke_tl", ignore_errors=True)   # hermetic: never reuse a prior run's state
os.makedirs(args.build_dir, exist_ok=True)

FAMS = [g.Family(f"ofl/fam{i}", f"Fam {i}", f"https://github.com/owner/repo{i}", "abc123",
                 None, False, [f"Fam{i}.ttf"]) for i in range(5)]

# ---- monkeypatches: simulate the heavy ops cheaply ----
def fake_ensure_gf(path, on_progress=None):
    if on_progress: on_progress("cloning google/fonts (fake)…")
    time.sleep(0.3)
    return path
g.ensure_google_fonts = fake_ensure_gf

def fake_build_fontc(dest, on_progress=None):
    for m in ("cloning fontc…", "cargo build --release…"):
        if on_progress: on_progress(m)
        time.sleep(0.3)
    return "/tmp/_smoke_tl/fontc/target/release/fontc"
g.build_fontc_from_source = fake_build_fontc

g.discover = lambda gf: (FAMS, 12, 7)

def fake_populate(urls, archive, jobs, on_progress=None, stop=None, clone_lock=None):
    added = []
    for i, u in enumerate(sorted(set(urls)), 1):
        time.sleep(0.15)
        st = "added" if i % 4 else "failed"
        (added if st == "added" else []).append(u)
        if on_progress: on_progress(i, len(urls), u, st)
    return added, [], 0
g.populate_archive = fake_populate

def fake_build_one(self, wid, slug):
    time.sleep(0.2)
    with self.lock:
        r = self.results[slug]
        r.status, r.backend, r.started, r.ended = "built", "fontmake", time.time(), time.time()
g.Orchestrator._build_one = fake_build_one

# ---- run and watch the snapshot ----
orch = g.Orchestrator(args)
print("initial tasks:", [(t.key, t.status) for t in orch.tasks])
orch.run()
seen_phases, max_archive = [], 0
for _ in range(400):
    s = orch.snapshot()
    if not seen_phases or seen_phases[-1] != s["phase"]:
        seen_phases.append(s["phase"])
    max_archive = max(max_archive, len(s["archive_recent"]))
    if s["done"]:
        break
    time.sleep(0.1)
orch.join()
s = orch.snapshot()
print("phase sequence:", seen_phases)
print("final tasks:")
for t in s["tasks"]:
    print(f"  {t['status']:<8} {t['key']:<12} {t['done']}/{t['total']} "
          f"{t['elapsed']}s  {t['detail']}")
print("max archive_recent seen:", max_archive)
print("counts:", s["counts"])
assert [t["status"] for t in s["tasks"]] == ["done"] * 5, "all 5 tasks should be done"
assert [t["key"] for t in s["tasks"]] == ["clone_gf", "build_fontc", "discover", "archive", "build"]
assert seen_phases[:2] == ["clone_gf", "build_fontc"], seen_phases  # discover may be too fast to poll
assert seen_phases[-1] == "done"
# archive runs CONCURRENTLY with build (it no longer owns a phase), so "archive" is not in the
# phase sequence; the build phase is what follows discover:
assert "build" in seen_phases, seen_phases
assert s["counts"]["built"] == 5, s["counts"]
assert max_archive > 0, "archive_recent should have grown"
print("\nSMOKE OK")
