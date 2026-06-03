"""_enqueue() reconciles stale in-flight results so the 'queued' counter stays coherent: a family
left 'queued'/'building' by a PRIOR run that isn't in THIS run's worklist becomes 'skipped'
('not selected this run'), instead of showing as perpetually pending. Built/failed are untouched;
worklist families are (re)queued."""
import os
import sys
import tempfile
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

d = tempfile.mkdtemp(prefix="_enq_")
os.makedirs(d + "/build", exist_ok=True)
args = types.SimpleNamespace(
    build_dir=d + "/build", google_fonts=None, archive=d + "/a", source="archive",
    archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=2, percent=100.0, only="",
    populate_archive=False, manage_venvs=False, base_python="python3", base_requirements=None,
    build_python="python3", timeout=None, rebuild=False, retry_failed=False, compare=False,
    keep_work=False, keep_fonts=True, mirror_missing=False, _want_build_fontc=False, _data_dir=d)
o = g.Orchestrator(args)

# this run's worklist: 2 families (one of which carries a stale 'queued' from before)
fams = [g.Family("ofl/a", "A", "u", "c", None, False, []),
        g.Family("ofl/b", "B", "u", "c", None, False, [])]
o.families = {f.slug: f for f in fams}
o.results = {
    "ofl/a": g.Result(slug="ofl/a", status="queued"),       # in worklist → re-queued
    "ofl/old1": g.Result(slug="ofl/old1", status="queued"),    # stale, not in worklist → skipped
    "ofl/old2": g.Result(slug="ofl/old2", status="building"),  # stale in-flight → skipped
    "ofl/done": g.Result(slug="ofl/done", status="built"),     # keep
    "ofl/bad": g.Result(slug="ofl/bad", status="failed"),      # keep
}

o._enqueue()

assert o.results["ofl/a"].status == "queued", o.results["ofl/a"].status
assert o.results["ofl/b"].status == "queued", o.results["ofl/b"].status
assert o.results["ofl/old1"].status == "skipped", o.results["ofl/old1"].status
assert o.results["ofl/old1"].note == "not selected this run", o.results["ofl/old1"].note
assert o.results["ofl/old2"].status == "skipped", o.results["ofl/old2"].status
assert o.results["ofl/done"].status == "built", "built must be preserved"
assert o.results["ofl/bad"].status == "failed", "failed must be preserved"
print("stale queued/building reconciled to skipped; built/failed preserved")

qitems = []
while not o.q.empty():
    qitems.append(o.q.get())
assert set(qitems) == {"ofl/a", "ofl/b"}, qitems
print("work queue holds exactly this run's worklist:", sorted(qitems))

# the snapshot's queued count now reflects only real pending work (2), with no stale ghosts
snap = o.snapshot()
assert snap["counts"]["queued"] == 2, snap["counts"]
assert snap["counts"]["skipped"] == 2, snap["counts"]
assert snap["counts"]["built"] == 1 and snap["counts"]["failed"] == 1, snap["counts"]
print("snapshot counts coherent:", snap["counts"])

print("\nENQUEUE-RECONCILE OK")
