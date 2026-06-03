"""Persistence across restarts ("new session"): the COHORT map (= which venvs are cached on disk)
and the FAILURE HISTORY both survive a fresh Orchestrator on the same build dir, and a failing
family's log is archived under logs/failed/ so a later rebuild can't erase how it broke."""
import os
import sys
import json
import types
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

root = Path(tempfile.mkdtemp(prefix="_persist_"))
bd = root / "build"
(bd / "logs").mkdir(parents=True)
(bd / "venvs" / "c-abc").mkdir(parents=True)
(bd / "venvs" / "c-abc" / ".gflib-installed").write_text("hash\n")     # a CACHED venv on disk


def args():
    return types.SimpleNamespace(
        build_dir=str(bd), google_fonts=None, archive=str(root / "a"), source="archive",
        archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=1, percent=100.0, only="",
        rebuild=False, retry_failed=False, retry_category="", populate_archive=False, manage_venvs=False,
        base_python="python3", base_requirements=None, build_python="python3", timeout=None,
        compare=False, keep_work=False, keep_fonts=True, mirror_missing=False, _want_build_fontc=False,
        _data_dir=str(root))


os.makedirs(str(root / "a"), exist_ok=True)

# ---- session 1: assign cohorts, write a family log, fail a family ----
o1 = g.Orchestrator(args())
o1.families = {"ofl/x": g.Family("ofl/x", "Fam X", "u", "c", None, False, ["X.ttf"]),
               "ofl/y": g.Family("ofl/y", "Fam Y", "u", "c", None, False, ["Y.ttf"])}
o1.results = {"ofl/x": g.Result(slug="ofl/x", status="building")}
(bd / "logs" / "ofl__x.log").write_text("# ofl/x\nbuild[fontmake]: FAIL boom — qcurve\n")
o1._note_cohort("ofl/x", "c-abc", "fontmake==1\n")
o1._note_cohort("ofl/y", "c-abc", "fontmake==1\n")
o1._fail("ofl/x", "fontmake: produced no expected font files")

assert o1.failure_history and o1.failure_history[-1]["slug"] == "ofl/x", o1.failure_history
assert (bd / "failure-history.jsonl").is_file(), "failure history file written"
assert (bd / "logs" / "failed" / "ofl__x.log").is_file(), "failing log archived to logs/failed/"
o1.save_state()
st = json.loads((bd / "state.json").read_text())
assert "c-abc" in st["cohort_members"], "cohort map persisted to state.json"
print("session 1: failure recorded + log archived + cohort map persisted")

# ---- session 2: a fresh Orchestrator on the same build dir (a restart) ----
o2 = g.Orchestrator(args())                          # __init__ loads state + failure history
assert any(e["slug"] == "ofl/x" for e in o2.failure_history), "failure history survives the restart"
assert "c-abc" in o2._cohort_members and o2.cohorts.get("c-abc"), "cohort map restored on restart"
assert o2._cohort_members["c-abc"] == {"ofl/x", "ofl/y"}, o2._cohort_members
print("session 2: failure history + cohort map both restored from disk")

# the snapshot marks the cohort as cached (its venv is on disk) and exposes the persistent history
o2.families = o1.families
with o2.lock:
    o2._rebuild_cohorts()
snap = o2.snapshot()
co = {c["key"]: c for c in snap["cohorts"]}
assert co["c-abc"]["cached"] is True, ("cohort with a ready venv must show cached=True", co)
assert any(h["slug"] == "ofl/x" for h in snap["failure_history"]), "snapshot exposes failure_history"
print("snapshot: cohort cached=True (venv on disk) + failure_history present")

# a brand-new build dir has no cached venv -> cached=False, no history
(root / "fresh").mkdir()
o3 = g.Orchestrator(types.SimpleNamespace(**{**vars(args()), "build_dir": str(root / "fresh")}))
assert o3.failure_history == []
print("fresh build dir: empty history, nothing falsely cached")

print("\nPERSISTENCE OK")
