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
# the editable fields initialise from the config (no bare "None" values)
fields = g.CursesFrontend._cfg_init_fields(eff)
for f in fields:
    assert "None" not in str(f["value"]), (f["key"], f["value"])
os.unlink(cfgf)
print("config: falls back to persisted settings; fields initialise without bare 'None'")

# ---- retry_failed must PERSIST (it's in CONFIG_KEYS) so it survives a [C] reconfigure cycle ----
import types
assert "retry_failed" in g.CONFIG_KEYS, "retry_failed missing from CONFIG_KEYS → won't persist"
cfgf2 = tempfile.mktemp(suffix=".config")
saved = types.SimpleNamespace(source="metadata", google_fonts="/gf", archive="/a", build_dir="/b",
                              backend="auto", fontc_bin=None, jobs=4, percent=10.0, timeout=None,
                              populate_archive=True, manage_venvs=True, retry_failed=True,
                              base_requirements=None, compare=False, data_dir="/d")
g.save_config(g.Path(cfgf2), saved)
loaded = g.load_config(g.Path(cfgf2))
assert loaded.get("retry_failed") is True, f"retry_failed did not round-trip: {loaded}"
os.unlink(cfgf2)
print("config: retry_failed round-trips through save/load (persists across reconfigure)")

print("\nRESET+CONFIG OK")
