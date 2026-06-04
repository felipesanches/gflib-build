"""The headline disk figure must include BOTH the build dir AND the upstream-repo archive (the bare
mirrors live in their own tree, usually outside build_dir). _measure_archive() adds the archive — but
returns 0 when the archive is nested under build_dir, so it's never double-counted. The snapshot
exposes disk_archive_total alongside disk_build_total for the TUI/web headers."""
import os
import sys
import types
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g


def mk(args_archive, build="build"):
    root = Path(tempfile.mkdtemp(prefix="_disk_"))
    bd = root / build
    (bd / "logs").mkdir(parents=True)
    ar = root / args_archive if not os.path.isabs(args_archive) else Path(args_archive)
    ar.mkdir(parents=True, exist_ok=True)
    args = types.SimpleNamespace(
        build_dir=str(bd), google_fonts=None, archive=str(ar), source="archive", archive_rev="HEAD",
        backend="auto", fontc_bin=None, jobs=1, percent=100.0, only="", rebuild=False,
        retry_failed=False, retry_category="", populate_archive=False, manage_venvs=False,
        base_python="python3", base_requirements=None, build_python="python3", timeout=None,
        compare=False, keep_work=False, keep_fonts=True, mirror_missing=False,
        _want_build_fontc=False, _data_dir=str(root))
    return root, bd, ar, g.Orchestrator(args)


# --- archive OUTSIDE build_dir: both trees are measured and summed ---
root, bd, ar, o = mk("archive")
(bd / "out" ).mkdir(); (bd / "out" / "big.ttf").write_bytes(b"x" * 50000)
(ar / "owner").mkdir(); (ar / "owner" / "repo.git").mkdir()
(ar / "owner" / "repo.git" / "pack").write_bytes(b"y" * 120000)

b = o._measure_dir(bd)
a = o._measure_archive()
assert b >= 50000, ("build dir measured", b)
assert a >= 120000, ("archive measured separately", a)
print("build dir measured:", b, "| archive measured:", a)

o._build_total = b
o._archive_bytes = a
snap = o.snapshot()
assert snap["disk_build_total"] == b, snap["disk_build_total"]
assert snap["disk_archive_total"] == a, snap["disk_archive_total"]
assert snap["disk_build_total"] + snap["disk_archive_total"] >= 170000
print("snapshot exposes disk_build_total + disk_archive_total (summed for the header)")

# --- archive NESTED under build_dir: must NOT be double-counted (returns 0) ---
root2, bd2, _ar2, o2 = mk("build/archive")          # archive lives inside the build dir
(bd2 / "archive" / "owner").mkdir(parents=True)
(bd2 / "archive" / "owner" / "r.git").write_bytes(b"z" * 90000)
assert o2._measure_archive() == 0, "archive under build/ is already in the build total — don't double-count"
# but _measure_dir(build) still sees those bytes (proves they're not lost, just not double-counted)
assert o2._measure_dir(bd2) >= 90000
print("archive nested under build/ -> _measure_archive()==0 (no double-count); bytes still in build total")

# --- missing archive dir -> 0, no crash ---
root3, bd3, ar3, o3 = mk("archive")
import shutil
shutil.rmtree(ar3)
assert o3._measure_archive() == 0 and o3._measure_dir(ar3) == 0
print("missing archive dir -> 0, no crash")

print("\nDISK-USAGE OK")
