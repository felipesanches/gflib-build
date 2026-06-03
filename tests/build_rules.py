"""build_rules.json: per-family pre-build commands run (shell, cwd=extracted source, venv bin on
PATH) before gftools-builder/fontc/fontmake. Tests the loader + runner: a rule's commands run in
order in the work dir; a failing command returns a clear error; a family with no rule is a no-op."""
import os
import sys
import json
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# the shipped build_rules.json parses; every rule is well-formed (slug -> {pre_build: [str,...]})
shipped = Path(__file__).resolve().parent.parent / "build_rules.json"
assert shipped.is_file(), "build_rules.json must be version-controlled next to the script"
rules = g.load_build_rules(shipped)
for slug, rule in rules.items():
    assert slug.count("/") == 1, f"rule key should be a family slug: {slug}"
    cmds = rule.get("pre_build")
    assert isinstance(cmds, list) and cmds and all(isinstance(c, str) for c in cmds), (slug, rule)
# the known Cairo cases (generated .glyphs) are registered
assert "ofl/cairo" in rules and "makenormal" in rules["ofl/cairo"]["pre_build"][0]
assert "ofl/cairoplay" in rules and "makeplay" in rules["ofl/cairoplay"]["pre_build"][0]
print(f"shipped build_rules.json parses; {len(rules)} well-formed rule(s): {sorted(rules)}")

# loader reads the 'rules' object
tf = tempfile.mktemp(suffix=".json")
json.dump({"rules": {"ofl/x": {"pre_build": ["true"]}}, "_example": {"ignored": 1}}, open(tf, "w"))
rules = g.load_build_rules(tf)
assert rules == {"ofl/x": {"pre_build": ["true"]}}, rules
assert g.load_build_rules("/no/such/file") == {}
os.unlink(tf)
print("loader returns the rules map; missing file -> {}")

# run_pre_build runs the commands IN ORDER in the work dir
work = Path(tempfile.mkdtemp(prefix="_pb_"))
log = work / "fam.log"
log.write_text("")
rules = {"ofl/x": {"pre_build": ["echo first > a.txt", "echo second >> a.txt", "mkdir gen"]}}
err = g.run_pre_build("ofl/x", work, sys.executable, rules, log, None)
assert err == "", err
assert (work / "a.txt").read_text().split() == ["first", "second"], (work / "a.txt").read_text()
assert (work / "gen").is_dir()
print("pre-build commands ran in order, in the work dir")

# a no-op for an unregistered family
assert g.run_pre_build("ofl/none", work, sys.executable, {}, log, None) == ""
assert g.run_pre_build("ofl/none", work, sys.executable, rules, log, None) == ""
print("unregistered family -> no-op")

# a failing command returns a clear error (and stops the sequence)
rules = {"ofl/x": {"pre_build": ["exit 3", "touch SHOULD_NOT_EXIST"]}}
err = g.run_pre_build("ofl/x", work, sys.executable, rules, log, None)
assert "rc=3" in err and "pre-build failed" in err, err
assert not (work / "SHOULD_NOT_EXIST").exists(), "must stop after the first failing command"
print("failing command -> clear 'pre-build failed (rc=3)' error, sequence halted")

# the venv bin is first on PATH (so a script's `python`/`fontmake` is the pinned one)
err = g.run_pre_build("ofl/x", work, sys.executable, {"ofl/x": {"pre_build": [
    'test "$(command -v python3)" = "%s/python3" || test "$(command -v python3)" = "%s"'
    % (str(Path(sys.executable).resolve().parent), sys.executable)]}}, log, None)
assert err == "", "the build venv bin should be first on PATH: " + err
print("pre-build runs with the build venv bin first on PATH")

print("\nBUILD-RULES OK")
