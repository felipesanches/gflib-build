"""The [R] 'retry now' action. Live (daemon running): a {"retry": [slug]} control message re-queues
that family via apply_live/_requeue. Finished build (no daemon): _retry_argv builds a one-family
targeted '--only <slug> --rebuild --yes' re-exec. Only families the run knows how to build are
re-queued."""
import os
import sys
import tempfile
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# ---- _retry_argv: keep existing flags, drop one-shot/attach/prior --only, append targeted rebuild
argv = ["gflib_build.py", "--data-dir", "/d", "--attach", "--only", "ofl/old", "--ui", "curses",
        "--wizard", "--yes"]
out = g._retry_argv(argv, "ofl/new")
assert out[0] == "gflib_build.py" and "--data-dir" in out and "/d" in out and "--ui" in out
assert "--attach" not in out and "--wizard" not in out and "ofl/old" not in out
assert out[-4:] == ["--only", "ofl/new", "--rebuild", "--yes"]
assert "--only=ofl/x" not in g._retry_argv(["s", "--only=ofl/x"], "ofl/new")  # --only=VALUE form too
print("_retry_argv: drops prior --only / one-shot flags, appends targeted rebuild")

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
o.families = {"ofl/x": g.Family("ofl/x", "X", "u", "c", None, False, [])}
o.results = {"ofl/x": g.Result(slug="ofl/x", status="failed", error="venv: No module named 'gftools'")}

# _requeue only touches families this run knows how to build
n = o._requeue(["ofl/x", "ofl/unknown"])
assert n == 1, n
assert o.results["ofl/x"].status == "queued", o.results["ofl/x"].status
assert "ofl/unknown" not in o.results, "must not invent a result for an unknown family"
got = []
while not o.q.empty():
    got.append(o.q.get())
assert got == ["ofl/x"], got
print("_requeue: re-queues a known failed family; ignores unknown slugs")

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
