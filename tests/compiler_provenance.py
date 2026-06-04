"""M0 — record the compiler AND its exact version for every build attempt, success or failure. The
version commands run at most once per (backend, venv) and are cached; the value lands on the Result,
in the persistent failure history, and in the snapshot's built/failures lists + a 'tooling' summary."""
import os
import sys
import types
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# stub the version capture (no subprocess in the test) and count calls (must be cached)
calls = []
g.compiler_version_str = lambda backend, py, fontc: (calls.append((backend, py)) or
    {"fontc": "fontc 0.9.0 (git abc1234)", "fontmake": "fontmake 3.11.1"}.get(backend, backend))

root = Path(tempfile.mkdtemp(prefix="_m0_"))
bd = root / "build"; (bd / "logs").mkdir(parents=True); os.makedirs(root / "a", exist_ok=True)
args = types.SimpleNamespace(
    build_dir=str(bd), google_fonts=None, archive=str(root / "a"), source="archive", archive_rev="HEAD",
    backend="auto", fontc_bin="/some/fontc", jobs=1, percent=100.0, only="", rebuild=False,
    retry_failed=False, retry_category="", populate_archive=False, manage_venvs=False,
    base_python="python3", base_requirements=None, build_python="python3", timeout=None, compare=False,
    keep_work=False, keep_fonts=True, mirror_missing=False, _want_build_fontc=False, _data_dir=str(root))
o = g.Orchestrator(args)

# the version is captured once per (backend, venv) and cached
assert o._compiler_version("fontc", "/venv/py") == "fontc 0.9.0 (git abc1234)"
assert o._compiler_version("fontc", "/venv/py") == "fontc 0.9.0 (git abc1234)"   # cached
assert o._compiler_version("fontmake", "/venv/py") == "fontmake 3.11.1"
assert len(calls) == 2, ("version commands must be cached (run once per backend/venv)", calls)
print("compiler version captured once per (backend, venv) and cached:", calls)

# a FAILED attempt records the compiler + version (M0: failures too)
o.families = {"ofl/x": g.Family("ofl/x", "X", "u", "c", None, False, ["X.ttf"])}
o.results = {"ofl/x": g.Result(slug="ofl/x", status="building", backend="fontc",
                               compiler_version="fontc 0.9.0 (git abc1234)")}
(bd / "logs" / "ofl__x.log").write_text("build[fontc]: FAIL\n")
o._fail("ofl/x", "fontc: produced no expected font files")
h = o.failure_history[-1]
assert h["backend"] == "fontc" and h["compiler_version"] == "fontc 0.9.0 (git abc1234)", h
print("failure history records compiler + version:", h["backend"], "·", h["compiler_version"])

# a BUILT family carries its compiler_version; the snapshot exposes it + a tooling summary
o.results["ofl/y"] = g.Result(slug="ofl/y", status="built", backend="fontmake",
                              compiler_version="fontmake 3.11.1", out_bytes=2048)
snap = o.snapshot()
assert snap["tooling"] == {"fontc": "fontc 0.9.0 (git abc1234)", "fontmake": "fontmake 3.11.1"}, snap["tooling"]
fr = [f for f in snap["failures_recent"] if f["slug"] == "ofl/x"][0]
assert fr["compiler_version"] == "fontc 0.9.0 (git abc1234)" and fr["backend"] == "fontc"
bu = [b for b in snap["built_recent"] if b["slug"] == "ofl/y"][0]
assert bu["compiler_version"] == "fontmake 3.11.1"
print("snapshot tooling:", snap["tooling"])
print("built/failed lists carry compiler_version:", bu["compiler_version"], "/", fr["compiler_version"])

# dev-fontc commit hash is preserved in the version string
assert "git abc1234" in snap["tooling"]["fontc"], "dev fontc must record the source commit"
print("dev fontc version includes the source commit hash")

print("\nCOMPILER-PROVENANCE (M0) OK")
