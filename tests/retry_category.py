"""Re-trigger mechanisms: --retry-category re-attempts failures of a named cause (like --retry-failed
but targeted, e.g. re-trigger 'output name mismatch' after fixing collect_outputs), and --only @file
restricts the whole run to an explicit family list (which becomes the entire queue = priority)."""
import os
import sys
import types
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

d = tempfile.mkdtemp(prefix="_rc_")
os.makedirs(d + "/a", exist_ok=True)
args = types.SimpleNamespace(
    only="", rebuild=False, retry_failed=False, retry_category="", build_dir=d, google_fonts=None,
    archive=d + "/a", source="archive", backend="fontmake", fontc_bin=None, jobs=1, percent=100.0,
    populate_archive=False, manage_venvs=False, base_python="python3", base_requirements=None,
    build_python="python3", timeout=None, compare=False, keep_work=False, keep_fonts=True,
    mirror_missing=False, _want_build_fontc=False, _data_dir=d, archive_rev="HEAD")
o = g.Orchestrator(args)
o.families = {"ofl/x": g.Family("ofl/x", "X", "u", "c", None, False, ["X.ttf"])}
NPEF = "fontc: produced no expected font files"          # categorises as 'output name mismatch'
assert g.categorize_failure(NPEF)[0] == "output name mismatch"

# without --retry-category, a non-auto-retryable cause is NOT re-attempted
o.results = {"ofl/x": g.Result(slug="ofl/x", status="failed", error=NPEF)}
o._enqueue()
assert o._enqueued == 0, "output-name-mismatch is not auto-retried by default"

# naming the cause re-attempts exactly it
o.args.retry_category = "output name mismatch"
o.results = {"ofl/x": g.Result(slug="ofl/x", status="failed", error=NPEF)}
o.q = g.queue.Queue(); o._enqueue()
assert o._enqueued == 1 and o._enqueued_retries == 1, "--retry-category re-attempts the named cause"

# a DIFFERENT cause is still skipped (retry_category is precise, not a blanket retry)
o.args.retry_category = "missing system library"
o.results = {"ofl/x": g.Result(slug="ofl/x", status="failed", error=NPEF)}
o.q = g.queue.Queue(); o._enqueue()
assert o._enqueued == 0, "--retry-category only re-attempts the named cause(s)"
print("--retry-category: re-attempts exactly the named failure cause(s)")

# --only @file expansion (the logic main() applies): one slug per line, '#' comments + blanks ignored
f = tempfile.mktemp(suffix=".txt")
open(f, "w").write("# recover list\nofl/a\n\nofl/b   \n# skip\nofl/c\n")
lines = open(f).read().splitlines()
only = ",".join(l.strip() for l in lines if l.strip() and not l.strip().startswith("#"))
assert only == "ofl/a,ofl/b,ofl/c", only
print("--only @file: expands to", only)

print("\nRETRY-CATEGORY / ONLY-FILE OK")
