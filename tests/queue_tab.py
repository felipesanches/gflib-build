"""The Queue tab: each queued family is tagged with WHY it's queued — new (never built), retry
(after a failure), or rebuild (of a prior success). snapshot() exposes queued_list in priority order
(variable + larger families first). Those three are the only kinds a family can be queued under."""
import os
import sys
import types
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

d = tempfile.mkdtemp(prefix="_qt_")
os.makedirs(d + "/a", exist_ok=True)


def orch(rebuild=False, retry_failed=False):
    args = types.SimpleNamespace(
        only="", rebuild=rebuild, retry_failed=retry_failed, retry_category="", build_dir=d,
        google_fonts=None, archive=d + "/a", source="archive", backend="fontmake", fontc_bin=None,
        jobs=1, percent=100.0, populate_archive=False, manage_venvs=False, base_python="python3",
        base_requirements=None, build_python="python3", timeout=None, compare=False, keep_work=False,
        keep_fonts=True, mirror_missing=False, _want_build_fontc=False, _data_dir=d, archive_rev="HEAD")
    o = g.Orchestrator(args)
    o.families = {
        "ofl/fresh": g.Family("ofl/fresh", "Fresh", "u", "c", None, True, ["F[wght].ttf"]),   # variable
        "ofl/failed": g.Family("ofl/failed", "Failed", "u", "c", None, False, ["X.ttf"]),
        "ofl/done": g.Family("ofl/done", "Done", "u", "c", None, False, ["D.ttf"]),
    }
    return o


# default run: fresh -> new; failed (broken venv = auto-retryable) -> retry; built -> skipped
o = orch()
o.results = {
    "ofl/failed": g.Result(slug="ofl/failed", status="failed", error="venv: No module named 'gftools'"),
    "ofl/done": g.Result(slug="ofl/done", status="built", backend="fontmake"),
}
o._enqueue()
kinds = {s: r.queued_kind for s, r in o.results.items() if r.status == "queued"}
assert kinds == {"ofl/fresh": "new", "ofl/failed": "retry"}, kinds
assert o.results["ofl/done"].status == "built", "a built family is not re-queued without --rebuild"
print("default enqueue: fresh=new, failed=retry, built=skipped")

# --rebuild: the built family is re-queued as 'rebuild'; the failed one as 'retry'
o = orch(rebuild=True)
o.results = {
    "ofl/failed": g.Result(slug="ofl/failed", status="failed", error="some genuine build error"),
    "ofl/done": g.Result(slug="ofl/done", status="built", backend="fontmake"),
}
o._enqueue()
kinds = {s: r.queued_kind for s, r in o.results.items() if r.status == "queued"}
assert kinds == {"ofl/fresh": "new", "ofl/failed": "retry", "ofl/done": "rebuild"}, kinds
print("--rebuild enqueue: built=rebuild, failed=retry, fresh=new")

# snapshot exposes queued_list in priority order (variable 'fresh' first) with the kinds
snap = o.snapshot()
ql = snap["queued_list"]
assert {q["slug"] for q in ql} == {"ofl/fresh", "ofl/failed", "ofl/done"}, ql
assert ql[0]["slug"] == "ofl/fresh", ("variable family should sort first", ql)
assert {q["slug"]: q["kind"] for q in ql}["ofl/fresh"] == "new"
print("snapshot.queued_list (priority order, with kinds):", [(q["slug"], q["kind"]) for q in ql])

# the live [R] requeue tags kind from the prior status (built->rebuild, failed->retry)
o = orch()
o.results = {"ofl/done": g.Result(slug="ofl/done", status="built"),
             "ofl/failed": g.Result(slug="ofl/failed", status="failed", error="e")}
o._requeue(["ofl/done", "ofl/failed"])
assert o.results["ofl/done"].queued_kind == "rebuild" and o.results["ofl/failed"].queued_kind == "retry"
print("[R] requeue: built->rebuild, failed->retry")

print("\nQUEUE-TAB OK")
