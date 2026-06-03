"""Concurrency safety: the archive pre-warmer and the build workers share a per-repo clone
lock, so a repo is NEVER `git clone --mirror`'d twice even when several families share it and
the pre-warmer races them. Uses the REAL populate_archive / ensure_mirror / KeyedLocks with
`git` stubbed to count clones and create the mirror dir after a delay (to force contention)."""
import collections
import os
import sys
import threading
import time
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

ARCHIVE = "/tmp/_conc_clone/archive"
os.makedirs(ARCHIVE, exist_ok=True)

clone_counts = collections.Counter()
cc_lock = threading.Lock()


def fake_clone(url, dest, timeout=1800, stop=None):
    with cc_lock:
        clone_counts[url] += 1
    time.sleep(0.25)                          # hold the clone open to force racing cloners
    os.makedirs(dest, exist_ok=True)          # now mirror_path(...).is_dir() is True
    return (0, "", "")


g.git_clone_mirror = fake_clone               # the (interruptible) clone path
g.git = lambda argv, timeout=None, **kw: (0, "", "")   # cat-file -e (commit present), etc.

# 6 families over 3 distinct repos: r0 shared by 3, r1 by 2, r2 by 1 -> heavy contention on r0
FAMS = [
    g.Family("ofl/a", "A", "https://github.com/o/r0", "c", None, False, ["A.ttf"]),
    g.Family("ofl/b", "B", "https://github.com/o/r0", "c", None, False, ["B.ttf"]),
    g.Family("ofl/c", "C", "https://github.com/o/r0", "c", None, False, ["C.ttf"]),
    g.Family("ofl/d", "D", "https://github.com/o/r1", "c", None, False, ["D.ttf"]),
    g.Family("ofl/e", "E", "https://github.com/o/r1", "c", None, False, ["E.ttf"]),
    g.Family("ofl/f", "F", "https://github.com/o/r2", "c", None, False, ["F.ttf"]),
]
g.ensure_google_fonts = lambda p, on_progress=None: p
g.discover = lambda gf: (FAMS, 9, 3)


def build_one_mirror_only(self, wid, slug):   # exercise ONLY the mirror step (the lock under test)
    fam = self.families[slug]
    mirror, err = g.ensure_mirror(self.archive, fam.repo_url, fam.commit, True,
                                  clone_lock=self.clone_locks,
                                  on_clone=lambda u: self._note_mirrored(u, "added"))
    with self.lock:
        r = self.results[slug]
        if err:
            r.status, r.error = "failed", err
        else:
            r.status, r.backend, r.started, r.ended = "built", "fontmake", time.time(), time.time()


g.Orchestrator._build_one = build_one_mirror_only

args = types.SimpleNamespace(
    build_dir="/tmp/_conc_clone/build", google_fonts="/tmp/_conc_clone/gf", archive=ARCHIVE,
    source="metadata", archive_rev="HEAD", backend="fontmake", fontc_bin=None, jobs=4,
    percent=100.0, only="", populate_archive=True, manage_venvs=False, base_python="python3",
    base_requirements=None, build_python="python3", timeout=None, rebuild=False,
    retry_failed=False, compare=False, keep_work=False, keep_fonts=True, mirror_missing=False,
    _want_build_fontc=False, _data_dir="/tmp/_conc_clone")
os.makedirs(args.build_dir, exist_ok=True)

orch = g.Orchestrator(args)
orch.run()
for _ in range(400):
    if orch.snapshot()["done"]:
        break
    time.sleep(0.05)
orch.join()
s = orch.snapshot()
print("clone counts per repo:", dict(clone_counts))
print("counts:", s["counts"])
print("archive_recent:", [a["repo"] for a in s["archive_recent"]])
assert all(v == 1 for v in clone_counts.values()), f"a repo was cloned more than once: {clone_counts}"
assert set(clone_counts) == {"https://github.com/o/r0", "https://github.com/o/r1",
                             "https://github.com/o/r2"}, clone_counts
assert s["counts"]["built"] == 6, s["counts"]
# the live list shows each repo exactly once (de-duped across pre-warmer + workers)
assert sorted(a["repo"] for a in s["archive_recent"]) == ["o/r0", "o/r1", "o/r2"]
print("\nCONCURRENCY OK — no double-clones; per-repo lock + de-dup correct")
