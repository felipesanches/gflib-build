"""Self-healing dependency install: a base requirements file with an UNAVAILABLE pin (a stale
dev/typo'd version not on PyPI) must NOT fail the build — the venv installer drops just that
pin, lets pip backtrack to a compatible version, and records the relaxation. Unit-tests the
parser/relaxer (no network) + one real light install (tiny packages) to prove the retry loop."""
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

# ---- unit: parse pip's "can't satisfy" errors + relax just those pins ----
log = """
Collecting fontmake==3.11.1
ERROR: Could not find a version that satisfies the requirement gftools==0.9.100.dev4 (from versions: 0.9.98, 0.9.99)
ERROR: No matching distribution found for gftools==0.9.100.dev4
"""
assert g._parse_unsatisfiable(log) == {"gftools"}, g._parse_unsatisfiable(log)

lines = ["fontmake==3.11.1", "# comment", "gftools==0.9.100.dev4", "-r other.txt",
         "ufo2ft @ git+https://github.com/x/y", ""]
relaxed = g.relax_requirements(lines, {"gftools"})
assert relaxed[0] == "fontmake==3.11.1"                      # untouched
assert relaxed[2].startswith("gftools ") and "auto-relaxed" in relaxed[2]   # pin dropped
assert relaxed[3] == "-r other.txt" and relaxed[4].startswith("ufo2ft @")   # untouched
assert g._req_pkg_name("Django>=4") == "django" and g._req_pkg_name("# x") == ""
print("unit: parser + relaxer OK")

# ---- real (light) install: bad pin on a tiny pkg → must self-heal ----
work = tempfile.mkdtemp(prefix="_self_heal_")
req = os.path.join(work, "base.txt")
open(req, "w").write("six==99.0.0\nwheel\n")                 # six 99.0.0 does NOT exist on PyPI
vm = g.VenvManager(g.Path(work), sys.executable, g.Path(req))
py, err = vm._create("base", "")
print("install:", "OK" if not err else f"FAIL: {err}")
print("relaxations:", vm.relaxations)
assert not err, err
assert py and os.path.exists(py), py
# six actually got installed (relaxed to a real version), and the relaxation was recorded
import subprocess
out = subprocess.run([py, "-c", "import six; print(six.__version__)"], capture_output=True, text=True)
print("six version installed:", out.stdout.strip())
assert out.returncode == 0, out.stderr
assert any("six" in r for r in vm.relaxations), vm.relaxations
print("\nSELF-HEAL OK (unavailable pin auto-relaxed; install succeeded; recorded)")
