"""collect_outputs: find freshly-built fonts RECURSIVELY (gftools-builder writes to whatever
outputDir the config sets, e.g. fonts/<Family>/variable/, not a fixed shallow list), skip binaries
the repo SHIPS in its tree (older mtime than this build), and report 'extras' (fresh fonts whose
name matches no shipped binary). Validated end-to-end against a real Anek build."""
import os
import sys
import time
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

work = Path(tempfile.mkdtemp(prefix="_co_work_")) / "work"
work.mkdir()
out = Path(tempfile.mkdtemp(prefix="_co_out_")) / "out"          # OUTSIDE work (as in real builds)
now = time.time()


def mkttf(rel, mtime):
    p = work / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(b"\x00\x01\x00\x00" + b"x" * 200)
    os.utime(p, (mtime, mtime))
    return p


# a fresh build in a family-specific subdir NOT in the old FONT_SUBDIRS (the Anek case)
mkttf("fonts/AnekGurmukhi/variable/AnekGurmukhi[wdth,wght].ttf", now)
# a binary the repo ships, in its tree, with an OLD (commit-date) mtime
mkttf("fonts/shipped/Committed-Regular.ttf", now - 10_000_000)   # ~116 days old
# a fresh font whose name matches NO shipped binary (a naming mismatch)
mkttf("fonts/variable/Family-VF.ttf", now)
# a plain shallow exact match (the already-working case) must still work
mkttf("fonts/ttf/Exact-Regular.ttf", now)

shipped = ["AnekGurmukhi[wdth,wght].ttf", "Exact-Regular.ttf", "Committed-Regular.ttf"]

tot, found, extras = g.collect_outputs(work, out, shipped, since=now)
assert "AnekGurmukhi[wdth,wght].ttf" in found, "recursive: nested fresh font must be found"
assert "Exact-Regular.ttf" in found, "shallow exact match must still work"
assert "Committed-Regular.ttf" not in found, "a committed (old) binary must NOT be collected"
assert "Family-VF.ttf" in extras and "Family-VF.ttf" not in found, "fresh non-shipped font -> extras"
assert tot > 0 and (out / "AnekGurmukhi[wdth,wght].ttf").is_file(), "matched fonts are copied to out"
print("recursive + fresh-only filter + extras:", sorted(found), "| extras:", extras)

# since=0 disables the time filter (used outside the build path) -> committed font now collected
_, f2, _ = g.collect_outputs(work, Path(tempfile.mkdtemp(prefix="_co2_")) / "o", shipped, since=0)
assert "Committed-Regular.ttf" in f2, "since=0 must collect regardless of mtime"
print("since=0 disables the fresh-only filter:", sorted(f2))

# the new category for the diagnostic message
assert g.categorize_failure("fontmake: built fonts but names don't match shipped — got [...]")[0] \
    == "output name mismatch"
assert g.categorize_failure("fontc: produced no expected font files")[0] == "output name mismatch"
print("categorize: 'output name mismatch' for the new diagnostics")

print("\nCOLLECT-OUTPUTS OK")
