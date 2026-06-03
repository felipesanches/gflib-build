"""The [R] 'retry now' action: a {"retry": [slug]} control message re-queues that family via
apply_live/_requeue (the daemon lingers after completion, so this works even when the build shows
complete — no program re-exec). Only families the run knows how to build, and not already in
flight, are re-queued."""
import os
import sys
import tempfile
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# ---- live re-queue ----
d = tempfile.mkdtemp(prefix="_retry_")
os.makedirs(d + "/build", exist_ok=True)
args = types.SimpleNamespace(
    build_dir=d + "/build", google_fonts=None, archive=d + "/a", source="archive",
    archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=2, percent=100.0, only="",
    populate_archive=False, manage_venvs=False, base_python="python3", base_requirements=None,
    build_python="python3", timeout=None, rebuild=False, retry_failed=False, compare=False,
    keep_work=False, keep_fonts=True, mirror_missing=False, _want_build_fontc=False, _data_dir=d)
o = g.Orchestrator(args)
o.families = {s: g.Family(s, s, "u", "c", None, False, []) for s in ("ofl/x", "ofl/busy", "ofl/q")}
o.results = {
    "ofl/x": g.Result(slug="ofl/x", status="failed", error="venv: No module named 'gftools'"),
    "ofl/busy": g.Result(slug="ofl/busy", status="building"),   # in flight → must NOT requeue
    "ofl/q": g.Result(slug="ofl/q", status="queued"),           # already queued → must NOT requeue
}

# _requeue only touches known, NOT-in-flight families (never double-build a building/queued slug)
n = o._requeue(["ofl/x", "ofl/busy", "ofl/q", "ofl/unknown"])
assert n == 1, n
assert o.results["ofl/x"].status == "queued", o.results["ofl/x"].status
assert o.results["ofl/busy"].status == "building", "must not re-queue an in-flight family"
assert o.results["ofl/q"].status == "queued"
assert "ofl/unknown" not in o.results, "must not invent a result for an unknown family"
got = []
while not o.q.empty():
    got.append(o.q.get())
assert got == ["ofl/x"], got
print("_requeue: re-queues a known failed family; skips in-flight + unknown slugs")

# apply_live routes a {"retry": [...]} control message to _requeue (workers stubbed out)
o.results["ofl/x"] = g.Result(slug="ofl/x", status="failed", error="venv: No module named 'gftools'")
o._ensure_workers = lambda *_a, **_k: None             # don't spawn real workers in the test
o.apply_live({"retry": ["ofl/x"]})
assert o.results["ofl/x"].status == "queued", o.results["ofl/x"].status
assert any("retry" in m for m in o.control_log), o.control_log
print("apply_live: a retry control message re-queues the family and logs it")

# a finished/stopping build ignores live retries (must use the re-exec path instead)
o.stop.set()
o.results["ofl/x"] = g.Result(slug="ofl/x", status="failed", error="venv: No module named 'gftools'")
o.apply_live({"retry": ["ofl/x"]})
assert o.results["ofl/x"].status == "failed", "stopped build must not live-requeue"
print("apply_live: ignores retry once the build has stopped (re-exec handles that case)")

print("\nRETRY-ACTION OK")
