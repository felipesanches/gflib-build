"""Proactive in-build retry: a TRANSIENT failure (network/IO blip) is re-queued automatically a few
times (IN_BUILD_RETRIES) before being recorded as failed, so the user doesn't have to retry a blip.
Non-transient failures fail immediately. Also: the failure-cause summary names WHICH families."""
import os
import sys
import tempfile
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

d = tempfile.mkdtemp(prefix="_inbuild_")
os.makedirs(d + "/build/out", exist_ok=True)
args = types.SimpleNamespace(
    build_dir=d + "/build", google_fonts=None, archive=d + "/a", source="archive",
    archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=2, percent=100.0, only="",
    populate_archive=False, manage_venvs=False, base_python="python3", base_requirements=None,
    build_python="python3", timeout=None, rebuild=False, retry_failed=False, compare=False,
    keep_work=False, keep_fonts=True, mirror_missing=False, _want_build_fontc=False, _data_dir=d)
o = g.Orchestrator(args)
o.families = {s: g.Family(s, s, "u", "c", None, False, []) for s in ("ofl/t", "ofl/x")}

TRANSIENT = "mirror clone failed: fatal: fetch-pack: invalid index-pack output"
assert g.categorize_failure(TRANSIENT)[0] in g.IN_BUILD_RETRY_CATEGORIES

# a transient failure is auto-re-queued, not failed, up to IN_BUILD_RETRIES
o.results = {"ofl/t": g.Result(slug="ofl/t", status="building")}
for attempt in range(1, o.IN_BUILD_RETRIES + 1):
    o._fail("ofl/t", TRANSIENT)
    assert o.results["ofl/t"].status == "queued", (attempt, o.results["ofl/t"].status)
    assert o.results["ofl/t"].retries == attempt, (attempt, o.results["ofl/t"].retries)
    assert not o.q.empty() and o.q.get() == "ofl/t"
    o.results["ofl/t"].status = "building"            # a worker picks it up again
print(f"transient failure auto-re-queued {o.IN_BUILD_RETRIES}x (no user action)")

# once the budget is spent, it is recorded as failed for real
o._fail("ofl/t", TRANSIENT)
assert o.results["ofl/t"].status == "failed", o.results["ofl/t"].status
assert "ofl/t" in o.failures
assert o.q.empty(), "must not keep re-queuing past the budget"
print("after the retry budget, the transient failure is recorded as failed")

# a NON-transient (genuine) failure fails immediately — no wasteful in-build retry
o.results["ofl/x"] = g.Result(slug="ofl/x", status="building")
o._fail("ofl/x", "gftools.builder exit 1: KeyError 'instances'")
assert o.results["ofl/x"].status == "failed" and o.q.empty()
print("genuine build error fails immediately (not in-build retried)")

# the failure-cause summary names the affected families (so they're addressable)
snap = o.snapshot()
cats = {c["cat"]: c for c in snap["fail_categories"]}
assert "ofl/t" in cats["transient fetch error"]["families"], cats
assert "ofl/x" in cats["build error"]["families"], cats
print("fail_categories list the affected family slugs:",
      {k: v["families"] for k, v in cats.items()})

print("\nIN-BUILD-RETRY OK")
