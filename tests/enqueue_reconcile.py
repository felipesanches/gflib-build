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

# this run's worklist (in self.families): includes failures with both fixable and genuine causes
fams = [g.Family(s, s, "u", "c", None, False, []) for s in
        ("ofl/a", "ofl/venvfail", "ofl/transient", "ofl/builderr", "ofl/unreachable",
         "ofl/syslib", "ofl/both")]
o.families = {f.slug: f for f in fams}
o.results = {
    "ofl/a": g.Result(slug="ofl/a", status="queued"),            # in worklist → re-queued
    # failed families IN the worklist, with different causes:
    "ofl/venvfail": g.Result(slug="ofl/venvfail", status="failed",
                             error="fontmake: ... No module named 'gftools'"),    # fixable → retry
    "ofl/transient": g.Result(slug="ofl/transient", status="failed",
                              error="mirror clone failed: fatal: fetch-pack: invalid index-pack output"),
    "ofl/builderr": g.Result(slug="ofl/builderr", status="failed",
                             error="gftools.builder exit 1: KeyError 'instances'"),  # genuine → keep
    "ofl/unreachable": g.Result(slug="ofl/unreachable", status="failed",
                                error="mirror clone failed: remote: Repository not found"),  # keep
    "ofl/syslib": g.Result(slug="ofl/syslib", status="failed",       # needs apt → NOT auto-retried
                           error="venv: missing system library: cairo (install libcairo2-dev)"),
    # broken venv reported via the 'both' backend wrapper must still be recognised as fixable:
    "ofl/both": g.Result(slug="ofl/both", status="failed",
                         error="both backends failed — fontc: No module named 'gftools' || fontmake: No module named 'gftools'"),
    # stale in-flight from a prior run, NOT in this worklist:
    "ofl/old1": g.Result(slug="ofl/old1", status="queued"),
    "ofl/old2": g.Result(slug="ofl/old2", status="building"),
    "ofl/done": g.Result(slug="ofl/done", status="built"),       # not in worklist → untouched
}

o._enqueue()

# auto-retry: fixable failures (broken venv, transient fetch) are re-queued; genuine build errors
# and unreachable repos are kept failed (no retry_failed flag set)
assert o.results["ofl/a"].status == "queued", o.results["ofl/a"].status
assert o.results["ofl/venvfail"].status == "queued", "broken-venv failure must be retried"
assert o.results["ofl/transient"].status == "queued", "transient fetch failure must be retried"
assert o.results["ofl/both"].status == "queued", "broken venv via 'both' wrapper must be retried"
assert o.results["ofl/builderr"].status == "failed", "genuine build error must NOT auto-retry"
assert o.results["ofl/unreachable"].status == "failed", "unreachable repo must NOT auto-retry"
assert o.results["ofl/syslib"].status == "failed", "missing system library must NOT auto-retry (needs apt)"
print("auto-retry: fixable failures re-queued; genuine/unreachable/syslib kept failed")
assert o._enqueued_retries == 3, o._enqueued_retries     # venvfail + transient + both
print(f"retry count exposed for the UI: {o._enqueued_retries}")

# stale in-flight (not in worklist) reconciled to skipped; out-of-worklist built untouched
assert o.results["ofl/old1"].status == "skipped" and o.results["ofl/old1"].note == "not selected this run"
assert o.results["ofl/old2"].status == "skipped"
assert o.results["ofl/done"].status == "built"
print("stale queued/building reconciled to skipped; out-of-worklist built preserved")

qitems = []
while not o.q.empty():
    qitems.append(o.q.get())
assert set(qitems) == {"ofl/a", "ofl/venvfail", "ofl/transient", "ofl/both"}, qitems
print("work queue holds the worklist + retried failures:", sorted(qitems))

snap = o.snapshot()
assert snap["counts"]["queued"] == 4, snap["counts"]
assert snap["counts"]["failed"] == 3, snap["counts"]          # builderr + unreachable + syslib
assert snap["counts"]["skipped"] == 2 and snap["counts"]["built"] == 1, snap["counts"]
print("snapshot counts coherent:", snap["counts"])

# --- with retry_failed, even genuine build errors are re-attempted ---
o2 = g.Orchestrator(args)
o2.args.retry_failed = True
o2.families = {f.slug: f for f in fams}
o2.results = {"ofl/builderr": g.Result(slug="ofl/builderr", status="failed",
                                       error="gftools.builder exit 1: KeyError")}
o2._enqueue()
assert o2.results["ofl/builderr"].status == "queued", "retry_failed must re-attempt genuine errors too"
print("retry_failed forces genuine build errors to retry as well")

print("\nENQUEUE-RECONCILE + AUTO-RETRY OK")
