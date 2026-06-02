"""CLI harness: monkeypatch the heavy ops, then call gflib_build.main() with the real argv.
Used to validate --detach (daemon) + auto-attach-on-rerun without real clones/builds."""
import time, types, sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

FAMS = [g.Family(f"ofl/fam{i}", f"Fam {i}", f"https://github.com/owner/repo{i}", "abc123",
                 None, False, [f"Fam{i}.ttf"]) for i in range(6)]
g.ensure_google_fonts = lambda path, on_progress=None: (time.sleep(0.3) or path)
g.discover = lambda gf: (FAMS, 12, 6)
g.discover_from_archive = lambda *a, **k: (FAMS, 12, 6)

def fake_populate(urls, archive, jobs, on_progress=None, stop=None):
    for i, u in enumerate(sorted(set(urls)), 1):
        time.sleep(0.2)
        if on_progress: on_progress(i, len(urls), u, "added")
    return list(urls), [], 0
g.populate_archive = fake_populate
g.scan_cohorts = lambda fams, *a, **k: ({"base": [f.slug for f in fams]}, {"base": ""})

def fake_build_one(self, wid, slug):
    time.sleep(float(os.environ.get("FAKE_BUILD_SECS", "0.6")))
    with self.lock:
        r = self.results[slug]
        r.status, r.backend, r.started, r.ended = "built", "fontmake", time.time(), time.time()
g.Orchestrator._build_one = fake_build_one

g.main()
