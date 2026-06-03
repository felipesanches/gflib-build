"""--reset deletes the build dir (built assets + venvs) but NEVER the archive (append-only),
and refuses when the archive lives inside the build dir. The config tab falls back to the
persisted config file so it shows real settings, not None."""
import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# ---- reset: delete build, keep archive + gf ----
root = Path(tempfile.mkdtemp(prefix="_reset_"))
bd, ar, gf = root / "build", root / "archive", root / "google-fonts"
for d in (bd / "out", bd / "venvs" / "base", bd / "logs", ar / "owner" / "repo.git", gf / "ofl"):
    d.mkdir(parents=True)
(bd / "out" / "Foo.ttf").write_text("font")
(ar / "owner" / "repo.git" / "HEAD").write_text("ref")        # a precious archived repo
g.reset_build(bd, ar, gf, assume_yes=True)
assert not bd.exists(), "build dir should be deleted"
assert ar.exists() and (ar / "owner" / "repo.git" / "HEAD").exists(), "archive must be intact"
assert gf.exists(), "google/fonts clone kept"
print("reset: build deleted, archive + gf intact")

# ---- safety: refuse if the archive is INSIDE the build dir ----
bd2 = root / "build2"
ar2 = bd2 / "archive"
ar2.mkdir(parents=True)
try:
    g.reset_build(bd2, ar2, None, assume_yes=True)
    raise SystemExit("ERROR: reset should have refused (archive inside build dir)")
except SystemExit as e:
    assert "refusing" in str(e), e
    assert bd2.exists(), "must not delete when archive is inside"
print("reset: refused when archive is inside the build dir")

import shutil
shutil.rmtree(root, ignore_errors=True)

# ---- config tab reflects persisted settings (no None) when the live config is empty ----
cfgf = tempfile.mktemp(suffix=".config")
json.dump({"percent": 25.0, "jobs": 8, "backend": "fontc", "timeout": None,
           "populate_archive": True, "compare": False, "source": "metadata",
           "google_fonts": "/gf", "archive": "/a", "build_dir": "/b", "manage_venvs": True},
          open(cfgf, "w"))
eff = g.CursesFrontend._effective_config({"config": {}, "config_path": cfgf})
assert eff["percent"] == 25.0 and eff["jobs"] == 8 and eff["backend"] == "fontc", eff
# and the display never prints a bare "None"
for f in g.CursesFrontend.CFG_EDITABLE:
    s = g.CursesFrontend._cfg_disp(f, None)
    assert s != "None" and "None" not in s, (f["key"], s)
os.unlink(cfgf)
print("config: falls back to persisted settings; no bare 'None' in display")

print("\nRESET+CONFIG OK")
