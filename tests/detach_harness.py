"""End-to-end CLI test: monkeypatch the heavy ops, then call gflib_build.main() with a real argv
and --ui none, which runs the WHOLE pipeline headless in the foreground (no daemon, no TTY) and
blocks to completion. Validates that main() parses args, bootstraps non-interactively (--yes),
and drives clone_gf -> discover -> archive -> cohorts -> build to done. (True --detach double-fork
behaviour is process-level and is exercised manually, not here, to keep this test deterministic.)"""
import json, time, types, sys, os, shutil
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

TMP = "/tmp/_detach_harness"
shutil.rmtree(TMP, ignore_errors=True)                # hermetic: never reuse a prior run's state
os.makedirs(TMP, exist_ok=True)

FAMS = [g.Family(f"ofl/fam{i}", f"Fam {i}", f"https://github.com/owner/repo{i}", "abc123",
                 None, False, [f"Fam{i}.ttf"]) for i in range(6)]
g.ensure_google_fonts = lambda path, on_progress=None: (time.sleep(0.2) or path)
g.discover = lambda gf: (FAMS, 12, 6)
g.discover_from_archive = lambda *a, **k: (FAMS, 12, 6)


def fake_populate(urls, archive, jobs, on_progress=None, stop=None, clone_lock=None):
    for i, u in enumerate(sorted(set(urls)), 1):
        time.sleep(0.05)
        if on_progress:
            on_progress(i, len(urls), u, "added")
    return list(urls), [], 0
g.populate_archive = fake_populate
g.scan_cohorts = lambda fams, *a, **k: ({"base": [f.slug for f in fams]}, {"base": ""})


def fake_build_one(self, wid, slug):
    time.sleep(float(os.environ.get("FAKE_BUILD_SECS", "0.1")))
    with self.lock:
        r = self.results[slug]
        r.status, r.backend, r.started, r.ended = "built", "fontmake", time.time(), time.time()
g.Orchestrator._build_one = fake_build_one

# real argv: headless full run, bootstrap without prompting
sys.argv = ["gflib_build.py", "--data-dir", TMP, "--source", "metadata", "--ui", "none",
            "--yes", "--jobs", "3", "--no-manage-venvs", "--no-save-config"]
g.main()

# after a foreground --ui none run, the pipeline has completed; status.json is authoritative
status = json.load(open(os.path.join(TMP, "build", "status.json")))
print("phase:", status.get("phase"), " done:", status.get("done"), " counts:", status.get("counts"))
assert status.get("done") is True, status
assert status["counts"]["built"] == 6, status["counts"]
assert status["counts"]["failed"] == 0, status["counts"]
tasks = {t["key"]: t["status"] for t in status.get("tasks", [])}
print("tasks:", tasks)
assert tasks.get("clone_gf") == "done" and tasks.get("build") == "done", tasks
print("\nDETACH-HARNESS OK (headless main() pipeline ran to completion)")
