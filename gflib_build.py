#!/usr/bin/env python3
"""
gflib-build — from-scratch, archive-safe full-library build harness for Google Fonts,
with a live ncurses dashboard.

DESIGN GUARANTEES (read me)
---------------------------
1. Sources come ONLY from the bare mirrors in the repo archive, read with
   `git archive <commit>` (a read-only operation that streams the committed tree).
   We NEVER check out into the mirror, NEVER fetch into a working tree, and NEVER
   delete a mirror. The archive is treated as an append-only, read-only source of truth.
2. Every family builds in a FRESH throwaway extraction under  <build-dir>/work/<slug>,
   pre-cleaned of any committed build outputs, so every build is genuinely from scratch.
   The throwaway tree is deleted after the build (unless --keep-work) to reclaim space.
3. All produced assets (built fonts, logs, results, state) live under <build-dir>.
   Nothing is ever written into the source repos / archive.
4. Resumable: per-family status is persisted to <build-dir>/state.json; families that
   already succeeded (or failed, unless --retry-failed) are skipped on the next run.

The harness itself is pure Python 3.8+ standard library (curses, threading, subprocess).
The actual font compile is delegated to a SEPARATE build interpreter (--build-python),
i.e. a venv that has gftools / fontmake / fonttools / glyphsLib / ttfautohint installed.
See README.md for the laptop setup.

Build invocation mirrors googlefonts/fontc `gfonts_repro_crater` and the real
fontc_crater semantics, including the gftools.builder behaviour of chdir-ing to the
config file's parent directory (so external/override configs are copied to the repo
root before building).
"""

import argparse
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Dict, List, Optional, Tuple

LICENSE_DIRS = ("ofl", "ufl", "apache")
# Committed build outputs we wipe from the throwaway tree before building, so the
# compile regenerates everything from sources (never inside the archive/repos).
OUTPUT_DIRS_TO_CLEAN = (
    "fonts", "instance_ufos", "instance_ufo", "master_ufo", "master_ufos",
    "variable_ttf", "variable", "build", "out", "output",
)
CONFIG_CANDIDATES = ("sources/config.yaml", "sources/config.yml", "config.yaml", "config.yml")
FONT_SUBDIRS = ("fonts/ttf", "fonts/variable", "fonts/otf", "fonts", ".")

RE_REPO = re.compile(r'repository_url:\s*"([^"]+)"')
RE_COMMIT = re.compile(r'commit:\s*"([0-9a-fA-F]{7,40})"')
RE_CONFIG = re.compile(r'config_yaml:\s*"([^"]+)"')
RE_BRANCH = re.compile(r'branch:\s*"([^"]+)"')
RE_NAME = re.compile(r'^name:\s*"([^"]+)"', re.M)
RE_FILENAME = re.compile(r'filename:\s*"([^"]+)"')


# --------------------------------------------------------------------------- model

@dataclass
class Family:
    slug: str                      # e.g. "ofl/dmsans"
    name: str
    repo_url: str
    commit: str
    branch: str
    config_yaml: Optional[str]     # path inside the upstream repo, or None
    has_override: bool             # google/fonts/<slug>/config.yaml exists
    shipped_fonts: List[str]       # filenames shipped in google/fonts


@dataclass
class Result:
    slug: str
    status: str = "queued"         # queued|building|built|failed|skipped
    started: float = 0.0
    ended: float = 0.0
    worker: int = -1
    error: str = ""                # one-line summary on failure
    log: str = ""                  # path to full log (relative to build dir)
    out_bytes: int = 0
    compare: str = ""              # "" | identical | differ | missing (with --compare)
    config_used: str = ""

    def dur(self) -> float:
        if self.started == 0:
            return 0.0
        return (self.ended or time.time()) - self.started


# ----------------------------------------------------------------------- discovery

def parse_metadata(meta_path: Path) -> Optional[Tuple[str, str, str, Optional[str], str, List[str]]]:
    """Return (name, repo_url, commit, config_yaml, branch, [shipped_fonts]) or None."""
    try:
        txt = meta_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    if "source {" not in txt and "source{" not in txt:
        return None
    m_repo = RE_REPO.search(txt)
    if not m_repo:
        return None
    m_commit = RE_COMMIT.search(txt)
    m_cfg = RE_CONFIG.search(txt)
    m_branch = RE_BRANCH.search(txt)
    m_name = RE_NAME.search(txt)
    fonts = RE_FILENAME.findall(txt)
    return (
        m_name.group(1) if m_name else meta_path.parent.name,
        m_repo.group(1),
        m_commit.group(1) if m_commit else "",
        m_cfg.group(1) if m_cfg else None,
        m_branch.group(1) if m_branch else "main",
        fonts,
    )


def discover(google_fonts: Path) -> Tuple[List[Family], int, int]:
    """Return (buildable families, total_with_source, skipped_no_config)."""
    fams: List[Family] = []
    total = 0
    skipped = 0
    for lic in LICENSE_DIRS:
        base = google_fonts / lic
        if not base.is_dir():
            continue
        for meta in sorted(base.glob("*/METADATA.pb")):
            parsed = parse_metadata(meta)
            if parsed is None:
                continue
            total += 1
            name, repo, commit, cfg, branch, fonts = parsed
            slug = f"{lic}/{meta.parent.name}"
            has_override = (google_fonts / slug / "config.yaml").is_file()
            # Buildable = has an override OR an in-repo config_yaml. (Auto-discovery in
            # the repo is attempted later as a fallback even when config_yaml is unset,
            # but families with neither override nor config_yaml are usually unbuildable.)
            buildable = has_override or bool(cfg)
            if not buildable:
                skipped += 1
                continue
            if not commit:
                skipped += 1
                continue
            fams.append(Family(slug, name, repo, commit, cfg, has_override, fonts))
    return fams, total, skipped


# --------------------------------------------------------------------- mirror utils

def mirror_path(archive: Path, repo_url: str) -> Path:
    """Map a repository_url to its bare mirror path: <archive>/<owner>/<repo>.git."""
    u = repo_url.strip().rstrip("/")
    u = re.sub(r"^https?://", "", u)
    u = re.sub(r"^git@([^:]+):", r"\1/", u)   # git@github.com:owner/repo -> github.com/owner/repo
    if u.endswith(".git"):
        u = u[:-4]
    parts = u.split("/")
    # last two path segments == owner/repo (handles github.com/owner/repo;
    # nested gitlab groups collapse to the final two segments to match the archive layout)
    owner, repo = parts[-2], parts[-1]
    return archive / owner / f"{repo}.git"


def git(args: List[str], cwd: Optional[Path] = None, timeout: int = 600) -> Tuple[int, str, str]:
    p = subprocess.run(
        ["git"] + args, cwd=str(cwd) if cwd else None,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout,
    )
    return p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")


def ensure_mirror(archive: Path, repo_url: str, commit: str, mirror_missing: bool) -> Tuple[Optional[Path], str]:
    """Ensure the bare mirror exists and contains `commit`. NEVER deletes anything.
    Returns (mirror_path, "") on success or (None, error)."""
    mp = mirror_path(archive, repo_url)
    if not mp.is_dir():
        if not mirror_missing:
            return None, f"mirror absent: {mp.name} (use --mirror-missing to clone)"
        mp.parent.mkdir(parents=True, exist_ok=True)
        rc, _, err = git(["clone", "--mirror", repo_url, str(mp)], timeout=1800)
        if rc != 0:
            return None, f"mirror clone failed: {err.strip().splitlines()[-1] if err.strip() else rc}"
    # commit present?
    rc, _, _ = git(["--git-dir", str(mp), "cat-file", "-e", f"{commit}^{{commit}}"])
    if rc != 0:
        # try a read-only refs update (adds refs, never deletes), then recheck
        git(["--git-dir", str(mp), "remote", "update", "--prune"], timeout=1800)
        rc, _, _ = git(["--git-dir", str(mp), "cat-file", "-e", f"{commit}^{{commit}}"])
        if rc != 0:
            return None, f"commit {commit[:10]} not in mirror {mp.name}"
    return mp, ""


def extract_tree(mirror: Path, commit: str, dest: Path, timeout: int) -> str:
    """Stream the committed tree at `commit` into `dest` via `git archive` (read-only on
    the mirror). Returns "" on success or an error string."""
    if dest.exists():
        shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True, exist_ok=True)
    # git --git-dir=<mirror> archive <commit> | tar -x -C dest
    git_p = subprocess.Popen(
        ["git", "--git-dir", str(mirror), "archive", "--format=tar", commit],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    tar_p = subprocess.Popen(
        ["tar", "-x", "-C", str(dest)], stdin=git_p.stdout, stderr=subprocess.PIPE,
    )
    git_p.stdout.close()
    try:
        _, tar_err = tar_p.communicate(timeout=timeout)
        _, git_err = git_p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        git_p.kill(); tar_p.kill()
        return "extract timed out"
    if git_p.returncode != 0:
        return f"git archive failed: {git_err.decode('utf-8','replace').strip()[:200]}"
    if tar_p.returncode != 0:
        return f"tar extract failed: {tar_err.decode('utf-8','replace').strip()[:200]}"
    return ""


def preclean_outputs(work: Path) -> None:
    """Remove committed build-output dirs from the throwaway tree (never the archive)."""
    for d in OUTPUT_DIRS_TO_CLEAN:
        p = work / d
        if p.is_dir():
            shutil.rmtree(p, ignore_errors=True)


# ------------------------------------------------------------------------- building

def resolve_config(google_fonts: Path, fam: Family, work: Path) -> Tuple[Optional[Path], str, str]:
    """Resolve the config to build, honoring gftools.builder's chdir-to-config-parent.
    For an external override, copy it INTO the extracted repo root so its repo-root-relative
    `sources:` resolve. Returns (config_path, label, error)."""
    # Priority 1: google/fonts override -> copy into repo root.
    override = google_fonts / fam.slug / "config.yaml"
    if override.is_file():
        dest = work / "__gflib_override_config.yaml"
        try:
            shutil.copyfile(override, dest)
        except OSError as e:
            return None, "", f"could not stage override config: {e}"
        return dest, f"override:{fam.slug}/config.yaml", ""
    # Priority 2: in-repo config_yaml from METADATA.
    if fam.config_yaml:
        p = work / fam.config_yaml
        if p.is_file():
            return p, fam.config_yaml, ""
    # Priority 3: auto-discover in the repo.
    for cand in CONFIG_CANDIDATES:
        p = work / cand
        if p.is_file():
            return p, cand, ""
    return None, "", "no config.yaml found (no override, no in-repo config)"


def run_builder(build_python: str, config_path: Path, work: Path, log_path: Path, timeout: int) -> Tuple[bool, str]:
    """Run gftools.builder; full output goes to log_path. Returns (ok, error_summary)."""
    env = dict(os.environ)
    env["SOURCE_DATE_EPOCH"] = "0"  # reproducible timestamps (matches crater)
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with open(log_path, "wb") as logf:
        logf.write(f"# {config_path}\n# cwd={work}\n\n".encode())
        logf.flush()
        try:
            p = subprocess.run(
                [build_python, "-m", "gftools.builder", str(config_path)],
                cwd=str(work), env=env, stdout=logf, stderr=subprocess.STDOUT, timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            return False, f"build timed out after {timeout}s"
        except OSError as e:
            return False, f"could not launch builder: {e}"
    if p.returncode != 0:
        return False, _last_error_line(log_path) or f"gftools.builder exit {p.returncode}"
    return True, ""


def _last_error_line(log_path: Path) -> str:
    try:
        lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return ""
    for ln in reversed(lines):
        s = ln.strip()
        if s and ("Error" in s or "error" in s or "Exception" in s or "Traceback" in s or "FAILED" in s):
            return s[:200]
    return (lines[-1].strip()[:200] if lines else "")


def collect_outputs(work: Path, out_dir: Path, shipped: List[str]) -> Tuple[int, Dict[str, Path]]:
    """Copy built fonts to out_dir; return (total_bytes, {filename: built_path})."""
    found: Dict[str, Path] = {}
    out_dir.mkdir(parents=True, exist_ok=True)
    total = 0
    # Prefer matching the shipped filenames; else collect any produced ttf/otf.
    want = set(shipped)
    for sub in FONT_SUBDIRS:
        d = work / sub
        if not d.is_dir():
            continue
        for f in d.iterdir():
            if not f.is_file() or f.suffix.lower() not in (".ttf", ".otf"):
                continue
            if want and f.name not in want:
                continue
            if f.name in found:
                continue
            dst = out_dir / f.name
            try:
                shutil.copyfile(f, dst)
                total += dst.stat().st_size
                found[f.name] = dst
            except OSError:
                pass
    return total, found


def sha256(path: Path) -> str:
    import hashlib
    h = hashlib.sha256()
    try:
        with open(path, "rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                h.update(chunk)
    except OSError:
        return ""
    return h.hexdigest()


def compare_to_shipped(google_fonts: Path, fam: Family, built: Dict[str, Path]) -> str:
    """sha256 built vs shipped binary. Returns identical|differ|missing."""
    if not fam.shipped_fonts:
        return ""
    fam_dir = google_fonts / fam.slug
    all_identical = True
    any_present = False
    for fn in fam.shipped_fonts:
        ref = fam_dir / fn
        if not ref.is_file():
            continue
        b = built.get(fn)
        if b is None:
            return "missing"
        any_present = True
        if sha256(ref) != sha256(b):
            all_identical = False
    if not any_present:
        return "missing"
    return "identical" if all_identical else "differ"


# --------------------------------------------------------------------- orchestrator

class Orchestrator:
    def __init__(self, args, families: List[Family], total_with_source: int, skipped_no_config: int):
        self.args = args
        self.build_dir = Path(args.build_dir)
        self.google_fonts = Path(args.google_fonts)
        self.archive = Path(args.archive)
        self.families = {f.slug: f for f in families}
        self.total_with_source = total_with_source
        self.skipped_no_config = skipped_no_config

        self.lock = threading.Lock()
        self.results: Dict[str, Result] = {}
        self.q: "queue.Queue[str]" = queue.Queue()
        self.stop = threading.Event()
        self.paused = threading.Event()
        self.start_time = time.time()
        self.disk_baseline = self._disk_used()
        self.failures: List[str] = []      # slugs, newest last
        self.workers: List[threading.Thread] = []

        self._load_state()
        self._enqueue()

    # ---- state persistence
    @property
    def state_path(self) -> Path:
        return self.build_dir / "state.json"

    def _load_state(self):
        if self.state_path.is_file():
            try:
                data = json.loads(self.state_path.read_text())
                for slug, r in data.get("results", {}).items():
                    self.results[slug] = Result(**r)
            except Exception:
                pass

    def save_state(self):
        with self.lock:
            data = {
                "saved_at": time.time(),
                "build_dir": str(self.build_dir),
                "results": {s: asdict(r) for s, r in self.results.items()},
            }
        tmp = self.state_path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(data, indent=1))
        tmp.replace(self.state_path)

    def _enqueue(self):
        only = set(self.args.only.split(",")) if self.args.only else None
        for slug, fam in self.families.items():
            if only and slug not in only:
                continue
            prev = self.results.get(slug)
            if prev and not self.args.rebuild:
                if prev.status == "built":
                    continue
                if prev.status == "failed" and not self.args.retry_failed:
                    continue
            self.results[slug] = Result(slug=slug, status="queued")
            self.q.put(slug)

    # ---- counters
    def counts(self) -> Dict[str, int]:
        with self.lock:
            c = {"built": 0, "failed": 0, "building": 0, "queued": 0, "skipped": 0}
            for r in self.results.values():
                c[r.status] = c.get(r.status, 0) + 1
            return c

    def building_now(self) -> List[Result]:
        with self.lock:
            return sorted([r for r in self.results.values() if r.status == "building"],
                          key=lambda r: r.started)

    def recent_failures(self, n: int) -> List[Result]:
        with self.lock:
            return [self.results[s] for s in self.failures[-n:] if s in self.results][::-1]

    def _disk_used(self) -> int:
        try:
            return shutil.disk_usage(self.build_dir).used
        except OSError:
            return 0

    def disk_delta(self) -> int:
        return max(0, self._disk_used() - self.disk_baseline)

    def disk_free(self) -> int:
        try:
            return shutil.disk_usage(self.build_dir).free
        except OSError:
            return 0

    # ---- worker
    def worker(self, wid: int):
        while not self.stop.is_set():
            if self.paused.is_set():
                time.sleep(0.2)
                continue
            try:
                slug = self.q.get(timeout=0.3)
            except queue.Empty:
                if self._all_done():
                    return
                continue
            try:
                self._build_one(wid, slug)
            finally:
                self.q.task_done()

    def _all_done(self) -> bool:
        with self.lock:
            return all(r.status in ("built", "failed", "skipped")
                       for r in self.results.values())

    def _set(self, slug: str, **kw):
        with self.lock:
            r = self.results[slug]
            for k, v in kw.items():
                setattr(r, k, v)

    def _build_one(self, wid: int, slug: str):
        fam = self.families[slug]
        safe = slug.replace("/", "__")
        work = self.build_dir / "work" / safe
        out_dir = self.build_dir / "out" / safe
        log_rel = f"logs/{safe}.log"
        log_path = self.build_dir / log_rel
        self._set(slug, status="building", started=time.time(), worker=wid, ended=0.0,
                  error="", log=log_rel)

        def fail(msg: str):
            self._set(slug, status="failed", ended=time.time(), error=msg)
            with self.lock:
                self.failures.append(slug)
            shutil.rmtree(work, ignore_errors=True)
            self.save_state()

        # 1. mirror + commit (read-only on the archive)
        mirror, err = ensure_mirror(self.archive, fam.repo_url, fam.commit, self.args.mirror_missing)
        if err:
            return fail(err)
        # 2. pristine extraction into a throwaway dir (never touches the mirror)
        err = extract_tree(mirror, fam.commit, work, self.args.timeout)
        if err:
            return fail(err)
        # 3. wipe committed build outputs -> build truly from scratch
        preclean_outputs(work)
        # 4. resolve config (external override copied into the repo root)
        cfg, label, err = resolve_config(self.google_fonts, fam, work)
        if err:
            return fail(err)
        self._set(slug, config_used=label)
        # 5. build (delegated to the build interpreter)
        ok, err = run_builder(self.args.build_python, cfg, work, log_path, self.args.timeout)
        if not ok:
            return fail(err)
        # 6. collect outputs into the separate build dir
        nbytes, built = collect_outputs(work, out_dir, fam.shipped_fonts)
        cmp_label = ""
        if self.args.compare:
            cmp_label = compare_to_shipped(self.google_fonts, fam, built)
        if nbytes == 0 and fam.shipped_fonts:
            # built nothing matching the shipped fonts -> treat as failure
            return fail("build produced no matching font files")
        self._set(slug, status="built", ended=time.time(), out_bytes=nbytes, compare=cmp_label)
        # 7. reclaim space: drop the throwaway tree (keep out/ + logs/)
        if not self.args.keep_work:
            shutil.rmtree(work, ignore_errors=True)
        if not self.args.keep_fonts:
            shutil.rmtree(out_dir, ignore_errors=True)
        self.save_state()

    # ---- run
    def run(self):
        n = max(1, self.args.jobs)
        for i in range(n):
            t = threading.Thread(target=self.worker, args=(i + 1,), daemon=True)
            t.start()
            self.workers.append(t)

    def join(self):
        while any(t.is_alive() for t in self.workers):
            if self._all_done():
                self.stop.set()
            time.sleep(0.2)
        self.save_state()


# --------------------------------------------------------------------------- curses

def human(n: float) -> str:
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if abs(n) < 1024:
            return f"{n:.0f}{unit}" if unit == "B" else f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}PiB"


def hms(secs: float) -> str:
    secs = int(secs)
    return f"{secs // 3600:02d}:{(secs % 3600) // 60:02d}:{secs % 60:02d}"


def run_tui(orch: Orchestrator):
    import curses

    def draw(stdscr):
        curses.curs_set(0)
        stdscr.nodelay(True)
        try:
            curses.start_color(); curses.use_default_colors()
            curses.init_pair(1, curses.COLOR_GREEN, -1)
            curses.init_pair(2, curses.COLOR_RED, -1)
            curses.init_pair(3, curses.COLOR_YELLOW, -1)
            curses.init_pair(4, curses.COLOR_CYAN, -1)
        except curses.error:
            pass
        GREEN = curses.color_pair(1); RED = curses.color_pair(2)
        YEL = curses.color_pair(3); CYAN = curses.color_pair(4)

        total = len([1 for _ in orch.results]) or 1
        while True:
            ch = stdscr.getch()
            if ch in (ord("q"), ord("Q")):
                orch.stop.set()
                break
            if ch in (ord("p"), ord("P")):
                if orch.paused.is_set():
                    orch.paused.clear()
                else:
                    orch.paused.set()

            c = orch.counts()
            done = c["built"] + c["failed"]
            grand = sum(c.values()) or 1
            h, w = stdscr.getmaxyx()
            stdscr.erase()

            def put(y, x, s, attr=0):
                if 0 <= y < h and 0 <= x < w:
                    stdscr.addnstr(y, x, s, max(0, w - x - 1), attr)

            paused = " [PAUSED]" if orch.paused.is_set() else ""
            put(0, 0, f" Google Fonts library build{paused}", curses.A_BOLD)
            put(0, max(0, w - 40),
                f"elapsed {hms(time.time() - orch.start_time)}", curses.A_BOLD)
            put(1, 0, f" disk: +{human(orch.disk_delta())} used   free {human(orch.disk_free())}"
                      f"   jobs {orch.args.jobs}", CYAN)

            # stats line
            put(3, 0, " Built ", curses.A_BOLD)
            put(3, 7, f"{c['built']}", GREEN | curses.A_BOLD)
            put(3, 7 + len(str(c['built'])) + 1, f"/{grand}")
            seg = 24
            put(3, seg, "Failed ", curses.A_BOLD); put(3, seg + 7, f"{c['failed']}", RED | curses.A_BOLD)
            put(3, seg + 14, "Building ", curses.A_BOLD); put(3, seg + 23, f"{c['building']}", YEL | curses.A_BOLD)
            put(3, seg + 30, "Queued ", curses.A_BOLD); put(3, seg + 37, f"{c['queued']}")

            # progress bar
            barw = max(10, w - 4)
            filled = int(barw * done / grand)
            put(4, 1, "[" + "#" * filled + "-" * (barw - filled) + "]")
            put(4, max(2, (barw // 2)), f" {100 * done // grand}% ", curses.A_BOLD)

            # now building
            row = 6
            put(row, 0, " Now building ".ljust(w - 1, "-"), curses.A_BOLD); row += 1
            for r in orch.building_now()[: max(0, (h - row) // 2 - 2)]:
                fam = orch.families.get(r.slug)
                put(row, 1, f"w{r.worker:>2} {r.slug:<34} {hms(r.dur()):>8}  building…", YEL)
                row += 1
            if not orch.building_now():
                put(row, 1, "(idle)"); row += 1

            # recent failures
            row += 1
            put(row, 0, f" Recent failures ({c['failed']}) ".ljust(w - 1, "-"),
                curses.A_BOLD); row += 1
            for r in orch.recent_failures(max(0, h - row - 2)):
                put(row, 1, f"{r.slug:<34} {r.error}", RED)
                put(row, 1 + 34 + 1 + min(len(r.error), w), "", 0)
                row += 1

            put(h - 1, 0,
                " [q]uit  [p]ause/resume   logs: " + str(orch.build_dir / "logs"),
                curses.A_DIM)
            stdscr.refresh()

            if orch._all_done():
                # one final frame, then linger briefly so the user sees 100%
                stdscr.refresh()
                time.sleep(1.2)
                break
            time.sleep(0.25)

    curses.wrapper(draw)


# ------------------------------------------------------------------------- headless

def run_headless(orch: Orchestrator):
    last = 0.0
    while any(t.is_alive() for t in orch.workers):
        if orch._all_done():
            orch.stop.set()
            break
        now = time.time()
        if now - last > 2.0:
            c = orch.counts()
            sys.stderr.write(
                f"\r[{hms(now - orch.start_time)}] built {c['built']} failed {c['failed']} "
                f"building {c['building']} queued {c['queued']} "
                f"disk +{human(orch.disk_delta())}   "
            )
            sys.stderr.flush()
            last = now
        time.sleep(0.3)
    sys.stderr.write("\n")


# ----------------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser(
        description="From-scratch, archive-safe full-library build of Google Fonts with a live TUI.")
    ap.add_argument("--google-fonts", required=True, help="path to a google/fonts clone")
    ap.add_argument("--archive", required=True,
                    help="path to the bare-mirror repo archive ({owner}/{repo}.git)")
    ap.add_argument("--build-dir", required=True,
                    help="output dir for builds/logs/state (NOT inside any repo)")
    ap.add_argument("--build-python", default=sys.executable,
                    help="python interpreter of the build venv (gftools/fontmake/...)")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4, help="parallel builds")
    ap.add_argument("--timeout", type=int, default=1800, help="per-family build timeout (s)")
    ap.add_argument("--only", default="", help="comma-separated slugs to build (e.g. ofl/dmsans)")
    ap.add_argument("--compare", action="store_true",
                    help="sha256-compare built fonts to the shipped binaries")
    ap.add_argument("--mirror-missing", action="store_true",
                    help="clone any missing upstream repo into the archive (append-only)")
    ap.add_argument("--retry-failed", action="store_true", help="re-attempt previously failed families")
    ap.add_argument("--rebuild", action="store_true", help="ignore prior state; rebuild everything")
    ap.add_argument("--keep-work", action="store_true", help="keep throwaway extractions (uses space)")
    ap.add_argument("--keep-fonts", action="store_true", default=True,
                    help="keep built fonts under out/ (default on)")
    ap.add_argument("--discard-fonts", dest="keep_fonts", action="store_false",
                    help="delete built fonts after comparison (save space)")
    ap.add_argument("--no-tui", action="store_true", help="headless: stderr progress only")
    ap.add_argument("--list", action="store_true", help="just print the buildable worklist and exit")
    args = ap.parse_args()

    gf = Path(args.google_fonts)
    if not (gf / "ofl").is_dir():
        ap.error(f"--google-fonts {gf} has no ofl/ — is this a google/fonts clone?")
    if not Path(args.archive).is_dir():
        ap.error(f"--archive {args.archive} not found")
    Path(args.build_dir).mkdir(parents=True, exist_ok=True)
    for sub in ("work", "out", "logs"):
        (Path(args.build_dir) / sub).mkdir(exist_ok=True)

    families, total, skipped = discover(gf)
    if args.list:
        for f in families:
            print(f"{f.slug:<38} {('override' if f.has_override else f.config_yaml or '?'):<24} {f.repo_url}")
        print(f"\n{len(families)} buildable | {total} with source | {skipped} skipped (no config/commit)",
              file=sys.stderr)
        return

    orch = Orchestrator(args, families, total, skipped)
    print(f"Discovered {len(families)} buildable families "
          f"({total} with source, {skipped} skipped). "
          f"Queued {orch.q.qsize()} (resuming: {len(families) - orch.q.qsize()} already done).",
          file=sys.stderr)

    def on_sigint(signum, frame):
        orch.stop.set()
    signal.signal(signal.SIGINT, on_sigint)

    orch.run()
    try:
        if args.no_tui:
            run_headless(orch)
        else:
            run_tui(orch)
            # if the user quit the TUI, let in-flight finish
        orch.join()
    finally:
        orch.save_state()

    c = orch.counts()
    print(f"\nDONE: built {c['built']}, failed {c['failed']}, skipped {c['skipped']}. "
          f"disk +{human(orch.disk_delta())}. state: {orch.state_path}", file=sys.stderr)
    if c["failed"]:
        print("Failures (see logs/):", file=sys.stderr)
        for r in orch.recent_failures(10**9)[::-1]:
            print(f"  {r.slug}: {r.error}", file=sys.stderr)


if __name__ == "__main__":
    main()
