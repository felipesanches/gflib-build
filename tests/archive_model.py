"""The Archive data model: _archive_cloning marks a repo 'cloning now'; _note_mirrored resolves it
(added/failed -> recent log + done; present/skipped -> done only); the snapshot's 'archive' dict
reports total (off-thread), active, recent (last 30 min), and the pending queue."""
import os, sys, types, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

d = tempfile.mkdtemp(prefix="_arch_"); os.makedirs(d + "/a", exist_ok=True)
args = types.SimpleNamespace(build_dir=d, google_fonts=None, archive=d + "/a", source="archive",
    archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=1, percent=100.0, only="", rebuild=False,
    retry_failed=False, retry_category="", populate_archive=True, manage_venvs=False, base_python="python3",
    base_requirements=None, build_python="python3", timeout=None, compare=False, keep_work=False,
    keep_fonts=True, mirror_missing=False, _want_build_fontc=False, _data_dir=d)
o = g.Orchestrator(args)
U = ["https://github.com/o/a", "https://github.com/o/b", "https://github.com/o/c", "https://github.com/o/d"]
o._archive_urls = list(U)

o._archive_cloning(U[0])                                  # a starts cloning
o._note_mirrored(U[1], "present")                         # b already present (resolved, not 'recent')
o._note_mirrored(U[2], "added")                           # c newly mirrored (recent)
o._note_mirrored(U[3], "failed", "remote: not found 404") # d unreachable (recent + reason)
o._archive_total = 1302

snap = o.snapshot(); av = snap["archive"]
assert av["total"] == 1302, av
assert av["active"] == ["o/a"], av["active"]              # cloning now
assert [e["repo"] for e in av["recent"]] == ["o/d", "o/c"], av["recent"]   # newest first; not 'present'
assert any(e["status"] == "failed" and "404" in e["reason"] for e in av["recent"]), av["recent"]
# pending = worklist minus done minus active: a is active, b/c/d done -> none pending
assert av["pending"] == [] and av["pending_total"] == 0, av
print("active/recent/pending:", av["active"], [e["repo"] for e in av["recent"]], av["pending_total"])

# completing the clone of a removes it from active and adds it to recent
o._note_mirrored(U[0], "added")
av = o.snapshot()["archive"]
assert av["active"] == [] and "o/a" in [e["repo"] for e in av["recent"]], av
print("clone completion clears 'active' and lands in 'recent'")

# a fresh repo not yet started shows as pending
o._archive_urls = U + ["https://github.com/o/e"]
av = o.snapshot()["archive"]
assert av["pending"] == ["o/e"] and av["pending_total"] == 1, av
print("an unstarted worklist repo shows as pending:", av["pending"])
print("\nARCHIVE-MODEL OK")
