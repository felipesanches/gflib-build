"""The 22h-run failure fixes: (1) PIN_OVERRIDES forces compreffor>=0.5.6 + drops the fontbakery extra
up front (the old compreffor sdist can't build on Py3.13 → ~110 families); (2) collect_outputs also
scans the stray `../fonts` dir an override config.yaml writes to (~26 families that built but were
missed)."""
import os, sys, time, tempfile
from pathlib import Path
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# (1) pin overrides
lines = ["gftools==0.9.99", "compreffor==0.5.0", "fontbakery[googlefonts]==0.12.0", "fonttools==4.50"]
out, applied = g.apply_pin_overrides(lines)
assert "compreffor>=0.5.6" in out[1], out
assert out[2].startswith("fontbakery "), ("fontbakery extra dropped", out[2])
assert out[0] == "gftools==0.9.99" and out[3] == "fonttools==4.50", "untouched pins kept"
assert set(applied) == {"compreffor", "fontbakery"}, applied
print("apply_pin_overrides: compreffor>=0.5.6 forced, fontbakery extra dropped, others kept:", applied)

# (2) collect_outputs scans the stray ../fonts an override config writes to
root = Path(tempfile.mkdtemp(prefix="_pin_"))
work = root / "work" / "ofl__demo"; work.mkdir(parents=True)
stray = root / "work" / "fonts" / "ttf"; stray.mkdir(parents=True)   # = work.parent/fonts/...
(stray / "Demo[wght].ttf").write_bytes(b"FRESHFONT")                  # fresh build in the stray dir
out_dir = root / "out"
t0 = time.time() - 1
total, found, extras = g.collect_outputs(work, out_dir, ["Demo[wght].ttf"], since=t0)
assert "Demo[wght].ttf" in found, ("stray ../fonts output must be collected", found, extras)
assert (out_dir / "Demo[wght].ttf").is_file() and total > 0
print("collect_outputs: picked up the override config's ../fonts output:", list(found))

# committed (old) binaries in the stray dir are NOT collected (mtime filter still applies)
old = root / "work" / "fonts" / "Old.ttf"; old.write_bytes(b"OLD")
os.utime(old, (t0 - 10**7, t0 - 10**7))
_, found2, _ = g.collect_outputs(work, root / "out2", ["Old.ttf"], since=time.time())
assert "Old.ttf" not in found2, "stale committed binary in stray dir must be ignored"
print("collect_outputs: stale committed binaries in the stray dir are still ignored (mtime gate holds)")

print("\nPIN-OVERRIDES + STRAY-COLLECT OK")
