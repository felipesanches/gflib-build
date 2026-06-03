#!/usr/bin/env python3
"""
gflib-build — from-scratch, archive-safe, Rust-first full-library build harness for
Google Fonts, with a MODULAR (optional ncurses) live frontend.

See README.md for the full specification. Highlights:

ARCHIVE SAFETY (strict)
  * Sources are read ONLY with `git archive <commit>` from the bare mirrors — never a
    checkout, never a fetch into a tree, never any write into a mirror.
  * Archives are never deleted (missing repos may only be *added* with --mirror-missing).
  * Every build is from scratch: a fresh extraction, committed outputs pre-cleaned, and
    the extraction discarded afterwards. All assets land under --build-dir, never in a repo.

RUST FIRST
  * --backend auto tries fontc (Rust) first, falling back to fontmake (Python), and
    records which backend built each family (the migration metric).

DEPENDENCY COHORTS
  * With --manage-venvs, families are grouped by their repo requirements.txt and share
    one virtualenv per distinct dependency set.

MODULAR UI
  * The Orchestrator core is UI-agnostic. It exposes snapshot() and writes
    state.json + events.jsonl so ANY frontend can attach — built-in: curses / plain /
    json / none, selectable with --ui. A web UI can simply tail events.jsonl / state.json.

PARTIAL RUNS
  * --percent P builds only an evenly-spaced sample of the library (e.g. 5%) to validate
    the tool quickly.

The harness is pure Python 3.8+ stdlib. The font compile is delegated to a separate
build interpreter / venv (gftools.builder + fontmake, and/or the fontc binary).
"""

import argparse
import hashlib
import json
import locale
import math
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, asdict, field
from pathlib import Path
from typing import Callable, Dict, List, Optional, Tuple

LICENSE_DIRS = ("ofl", "ufl", "apache")
OUTPUT_DIRS_TO_CLEAN = (
    "fonts", "instance_ufos", "instance_ufo", "master_ufo", "master_ufos",
    "variable_ttf", "variable", "build", "out", "output",
)
CONFIG_CANDIDATES = ("sources/config.yaml", "sources/config.yml", "config.yaml", "config.yml")
FONT_SUBDIRS = ("fonts/ttf", "fonts/variable", "fonts/otf", "fonts", ".")
REQ_FILES = ("requirements.txt", "requirements.in")

RE_REPO = re.compile(r'repository_url:\s*"([^"]+)"')
RE_COMMIT = re.compile(r'commit:\s*"([0-9a-fA-F]{7,40})"')
RE_CONFIG = re.compile(r'config_yaml:\s*"([^"]+)"')
RE_NAME = re.compile(r'^name:\s*"([^"]+)"', re.M)
RE_FILENAME = re.compile(r'filename:\s*"([^"]+)"')


# =============================================================================== model

@dataclass
class Family:
    slug: str
    name: str
    repo_url: str
    commit: str
    config_yaml: Optional[str]
    has_override: bool
    shipped_fonts: List[str] = field(default_factory=list)

    @property
    def is_variable(self) -> bool:
        return any("[" in f for f in self.shipped_fonts)


@dataclass
class Result:
    slug: str
    status: str = "queued"           # queued|building|built|failed|skipped
    started: float = 0.0
    ended: float = 0.0
    worker: int = -1
    backend: str = ""                # fontc|fontmake
    cohort: str = ""                 # venv cohort key
    note: str = ""                   # transient ("installing deps", ...)
    error: str = ""
    log: str = ""
    out_bytes: int = 0
    out_missing: int = 0
    compare: str = ""
    config_used: str = ""
    timings: Dict[str, float] = field(default_factory=dict)   # op -> seconds (mirror/extract/build/…)
    # fontc→fontmake migration tracking
    fontc_error: str = ""    # why fontc failed (when it fell back to fontmake, or in 'both')
    fontc_ok: bool = False   # 'both' mode: did fontc build succeed
    fontmake_ok: bool = False
    vs: str = ""             # 'both' mode: fontc-vs-fontmake comparison (identical | differ:<tables>)

    def dur(self) -> float:
        if self.started == 0:
            return 0.0
        return (self.ended or time.time()) - self.started


@dataclass
class Task:
    """One step of the end-to-end pipeline, rendered as a live task-list line
    (clone google/fonts, build fontc, discover, populate archive, cohorts, build)."""
    key: str
    name: str
    status: str = "pending"          # pending|running|done|failed|skipped
    t0: float = 0.0
    t1: float = 0.0
    done: int = 0                    # progress numerator (0 if not measurable)
    total: int = 0                   # progress denominator (0 if not measurable)
    detail: str = ""

    def elapsed(self) -> float:
        if not self.t0:
            return 0.0
        return (self.t1 or time.time()) - self.t0


# =========================================================================== discovery

def parse_metadata(meta_path: Path):
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
    m_name = RE_NAME.search(txt)
    return (
        m_name.group(1) if m_name else meta_path.parent.name,
        m_repo.group(1),
        m_commit.group(1) if m_commit else "",
        m_cfg.group(1) if m_cfg else None,
        RE_FILENAME.findall(txt),
    )


def discover(google_fonts: Path) -> Tuple[List[Family], int, int]:
    """Return (buildable families, library_total, skipped). `library_total` counts every
    family in the library (all METADATA.pb); `skipped` = library_total - buildable, i.e.
    families with no gftools build config and/or no pinned commit (legacy/SFD/VFB sources,
    missing_config, no source block)."""
    fams: List[Family] = []
    library_total = 0
    for lic in LICENSE_DIRS:
        base = google_fonts / lic
        if not base.is_dir():
            continue
        for meta in sorted(base.glob("*/METADATA.pb")):
            library_total += 1
            parsed = parse_metadata(meta)
            if parsed is None:
                continue
            name, repo, commit, cfg, fonts = parsed
            slug = f"{lic}/{meta.parent.name}"
            has_override = (google_fonts / slug / "config.yaml").is_file()
            if not commit or not (has_override or cfg):
                continue
            fams.append(Family(slug, name, repo, commit, cfg, has_override, fonts))
    return fams, library_total, library_total - len(fams)


def discover_from_archive(archive: Path, rev: str, jobs: int) -> Tuple[List[Family], int, int]:
    """Archive-driven discovery: the worklist is every bare mirror in the archive, each at
    `rev` (default HEAD = the mirror's default-branch tip). The repo URL is read from the
    mirror's origin remote. No google/fonts needed; config is auto-discovered at build
    time and shipped-binary comparison is unavailable (there is no METADATA reference)."""
    from concurrent.futures import ThreadPoolExecutor
    mirrors = sorted(archive.glob("*/*.git"))

    def resolve(mirror: Path) -> Optional[Family]:
        owner, repo = mirror.parent.name, mirror.name[:-4]
        rc, sha, _ = git(["--git-dir", str(mirror), "rev-parse", "--verify", f"{rev}^{{commit}}"])
        if rc != 0 or not sha.strip():
            return None
        rc2, url, _ = git(["--git-dir", str(mirror), "config", "--get", "remote.origin.url"])
        repo_url = url.strip() if rc2 == 0 and url.strip() else f"https://github.com/{owner}/{repo}"
        return Family(f"{owner}/{repo}", repo, repo_url, sha.strip(), None, False, [])

    fams: List[Family] = []
    with ThreadPoolExecutor(max_workers=max(1, jobs)) as ex:
        for fam in ex.map(resolve, mirrors):
            if fam is not None:
                fams.append(fam)
    total = len(mirrors)
    return fams, total, total - len(fams)


def sample_evenly(items: List[Family], percent: float) -> List[Family]:
    """Deterministic, evenly-spaced sample across the (alphabetical) list, so a small
    percentage still spans the whole library rather than one corner of it."""
    if percent >= 100 or not items:
        return items
    k = max(1, math.ceil(len(items) * percent / 100.0))
    if k >= len(items):
        return items
    stride = len(items) / k
    picked = [items[min(len(items) - 1, int(i * stride))] for i in range(k)]
    # de-dup while preserving order (int() collisions at tiny k)
    seen, out = set(), []
    for f in picked:
        if f.slug not in seen:
            seen.add(f.slug)
            out.append(f)
    return out


# ========================================================================= mirror/git

def mirror_path(archive: Path, repo_url: str) -> Path:
    u = repo_url.strip().rstrip("/")
    u = re.sub(r"^https?://", "", u)
    u = re.sub(r"^git@([^:]+):", r"\1/", u)
    if u.endswith(".git"):
        u = u[:-4]
    parts = u.split("/")
    return archive / parts[-2] / f"{parts[-1]}.git"


def git(args: List[str], cwd: Optional[Path] = None, timeout: int = 600):
    p = subprocess.run(["git"] + args, cwd=str(cwd) if cwd else None,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    return p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")


# Network hiccups that a clone usually survives on a second try (vs. permanent errors like
# "repository not found" / "authentication failed" / "access denied", which must NOT be retried).
# Kept SPECIFIC on purpose: loose tokens like "500"/"tls"/"remote error:" false-match genuine build
# errors and permanent access failures (a build error whose log tail merely contains "500" must not
# be treated as a transient fetch and retried forever).
TRANSIENT_CLONE_ERRORS = (
    "invalid index-pack output", "fetch-pack", "early eof", "rpc failed", "unexpected disconnect",
    "the remote end hung up", "connection reset", "connection timed out", "operation timed out",
    "could not resolve host", "gnutls_handshake", "ssl connect error", "tls handshake",
    "returned error: 50", "returned error: 429",
)


def is_transient_clone_error(err: str) -> bool:
    low = (err or "").lower()
    return any(s in low for s in TRANSIENT_CLONE_ERRORS)


def categorize_failure(error: str):
    """Map a family's failure message to a short CAUSE + an actionable HINT, for the UI summary
    (so the user sees *why* the majority failed and *what to do*, not just a wall of red lines)."""
    e = error or ""
    low = e.lower()
    if ("no module named 'gftools'" in low or "no module named gftools" in low
            or "could not launch builder" in low or "module specification" in low):
        return ("broken dependency venv", "the cohort venv had a failed install — it's rebuilt from "
                "scratch and re-attempted automatically each time you start the build")
    if "missing system library" in low:
        return ("missing system library", "a package built from source needs a native -dev library "
                "(e.g. apt install libcairo2-dev pkg-config) — self-heal can't install system pkgs")
    if "pip install" in low or low.startswith("venv:"):
        return ("dependency install failed", "pip couldn't satisfy the cohort requirements; stale "
                "pins are auto-relaxed — see the cohort's .install.log")
    if is_transient_clone_error(e):
        return ("transient fetch error", "a network hiccup while cloning — retried automatically; "
                "re-run to try the rest")
    if "mirror absent" in low:
        return ("repo not mirrored", "turn on 'populate archive' (or --mirror-missing) so the "
                "upstream repo is cloned into the archive")
    if "not in mirror" in low:
        return ("stale archive mirror", "the recorded commit isn't in the local mirror — run "
                "git remote update on that repo in the archive")
    if "mirror clone failed" in low or "repository not found" in low:
        return ("repo unreachable", "the upstream repo may be private, renamed, or removed")
    if "harness error" in low:
        return ("internal/transient I/O", "a transient filesystem error — usually clears on re-run")
    if "timed out" in low or "timeout" in low:
        return ("build timed out", "raise or disable the per-build timeout")
    if "builder" in low or "fontmake" in low or "fontc" in low or "gftools" in low:
        return ("build error", "the source or build tooling failed — open the family log")
    return ("other", "open the family log for details")


# Failure causes that a fresh attempt can plausibly clear (a rebuilt venv, a retried clone, an
# updated mirror, …) — these are RE-ATTEMPTED automatically on the next build, so the system heals
# itself instead of treating "failed" as terminal. Causes that CANNOT change without human action
# outside the tool are NOT here (they'd just re-fail, expensively): a "missing system library"
# needs an apt install, "repo unreachable"/genuine build errors won't fix themselves. The explicit
# `retry_failed` option re-attempts even those (e.g. after the user installs the -dev package).
# Note: a "broken dependency venv" IS retried, which rebuilds the venv once; if that rebuild then
# fails for a missing system library, the new error is no longer auto-retryable — so a cohort that
# needs an apt package costs exactly ONE rebuild attempt, not one on every Start.
AUTO_RETRY_CATEGORIES = {
    "broken dependency venv", "dependency install failed", "transient fetch error",
    "stale archive mirror", "repo not mirrored", "internal/transient I/O",
}


def _clone_mirror_once(url: str, dest: str, timeout: int,
                       stop: "Optional[threading.Event]" = None):
    proc = subprocess.Popen(["git", "clone", "--mirror", "--quiet", url, dest],
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    deadline = time.time() + timeout
    aborted = False
    while proc.poll() is None:
        if (stop is not None and stop.is_set()) or time.time() > deadline:
            aborted = stop is not None and stop.is_set()
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
            break
        time.sleep(0.3)
    rc = proc.returncode if proc.returncode is not None else 1
    err = proc.stderr.read().decode("utf-8", "replace") if proc.stderr else ""
    if rc != 0:
        shutil.rmtree(dest, ignore_errors=True)        # never leave a partial mirror behind
        if aborted:
            err = "aborted"
        elif not err.strip():
            err = f"timed out after {timeout}s"
    return rc, aborted, err


def git_clone_mirror(url: str, dest: str, timeout: int = 1800,
                     stop: "Optional[threading.Event]" = None, attempts: int = 3,
                     on_retry: Optional[Callable[[int, str], None]] = None):
    """`git clone --mirror url dest`, but ABORTABLE and AUTO-RETRYING: polled against `stop` so a
    shutdown / --stop / build-completion terminates an in-flight clone promptly instead of blocking
    up to `timeout`. A failed attempt whose error is TRANSIENT (network hiccup such as
    'fetch-pack: invalid index-pack output') is retried up to `attempts` times with a short
    abortable backoff; permanent errors (repo missing/private) are not retried. `--quiet` keeps
    stderr tiny; on any failure the partial mirror dir is removed (never a half mirror)."""
    rc, aborted, err = 1, False, ""
    for attempt in range(1, max(1, attempts) + 1):
        rc, aborted, err = _clone_mirror_once(url, dest, timeout, stop)
        if rc == 0 or aborted:
            break
        if attempt >= attempts or not is_transient_clone_error(err):
            break
        if on_retry:
            on_retry(attempt, err.strip().splitlines()[-1] if err.strip() else str(rc))
        if stop is not None:
            if stop.wait(min(10, 2 * attempt)):        # abortable backoff
                aborted = True
                break
        else:
            time.sleep(min(10, 2 * attempt))
    if rc != 0 and not aborted and attempts > 1 and is_transient_clone_error(err):
        err = err.rstrip() + f"  (after {attempts} attempts)"
    return rc, "", err


def ensure_mirror(archive: Path, repo_url: str, commit: str, mirror_missing: bool,
                  clone_lock: "Optional[KeyedLocks]" = None,
                  on_clone: Optional[Callable[[str], None]] = None,
                  stop: "Optional[threading.Event]" = None):
    """Locate the bare mirror for repo_url (clone-on-demand if `mirror_missing`). `clone_lock`
    (shared with the archive pre-warmer) serializes cloning per repo so it's never done twice;
    `on_clone(repo_url)` fires only when THIS call actually performs the clone (for the live
    list); `stop` makes an in-flight clone abortable."""
    mp = mirror_path(archive, repo_url)
    if not mp.is_dir():
        if not mirror_missing:
            return None, f"mirror absent: {mp.name} (use --mirror-missing)"
        lk = clone_lock(repo_url) if clone_lock else None
        if lk:
            lk.acquire()
        try:
            if not mp.is_dir():           # re-check under the per-repo lock (the pre-warmer or
                mp.parent.mkdir(parents=True, exist_ok=True)   # another worker may have cloned it)
                rc, _, err = git_clone_mirror(repo_url, str(mp), timeout=1800, stop=stop)
                if rc != 0:
                    tail = err.strip().splitlines()[-1] if err.strip() else str(rc)
                    return None, f"mirror clone failed: {tail}"
                if on_clone:
                    on_clone(repo_url)
        finally:
            if lk:
                lk.release()
    rc, _, _ = git(["--git-dir", str(mp), "cat-file", "-e", f"{commit}^{{commit}}"])
    if rc != 0:
        git(["--git-dir", str(mp), "remote", "update", "--prune"], timeout=1800)
        rc, _, _ = git(["--git-dir", str(mp), "cat-file", "-e", f"{commit}^{{commit}}"])
        if rc != 0:
            return None, f"commit {commit[:10]} not in mirror {mp.name}"
    return mp, ""


def extract_tree(mirror: Path, commit: str, dest: Path, timeout: int) -> str:
    if dest.exists():
        shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True, exist_ok=True)
    gp = subprocess.Popen(["git", "--git-dir", str(mirror), "archive", "--format=tar", commit],
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    tp = subprocess.Popen(["tar", "-x", "-C", str(dest)], stdin=gp.stdout, stderr=subprocess.PIPE)
    gp.stdout.close()
    try:
        _, terr = tp.communicate(timeout=timeout)
        _, gerr = gp.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        gp.kill(); tp.kill()
        return "extract timed out"
    if gp.returncode != 0:
        return f"git archive failed: {gerr.decode('utf-8','replace').strip()[:200]}"
    if tp.returncode != 0:
        return f"tar extract failed: {terr.decode('utf-8','replace').strip()[:200]}"
    return ""


def preclean_outputs(work: Path) -> None:
    for d in OUTPUT_DIRS_TO_CLEAN:
        p = work / d
        if p.is_dir():
            shutil.rmtree(p, ignore_errors=True)
    for ninja in work.glob("build*.ninja"):
        try:
            ninja.unlink()
        except OSError:
            pass


GOOGLE_FONTS_URL = "https://github.com/google/fonts.git"
FONTC_URL = "https://github.com/googlefonts/fontc.git"
EXTRACT_TIMEOUT = 3600  # cap for `git archive` extraction (independent of the build timeout)


def detect_fontc() -> Optional[str]:
    """Best-effort auto-detect of a fontc binary: PATH, then common build locations."""
    p = shutil.which("fontc")
    if p:
        return p
    for c in (Path("fontc") / "target" / "release" / "fontc",
              Path.home() / "fontc" / "target" / "release" / "fontc",
              Path("..") / "fontc" / "target" / "release" / "fontc"):
        if c.is_file():
            return str(c.resolve())
    return None


def detect_archive(data_dir: Path) -> Optional[str]:
    """Best-effort auto-detect of a pre-existing repo archive (a dir of {owner}/{repo}.git)."""
    for c in (data_dir / "archive", Path("repo_archive"), Path("archive"),
              Path.home() / "repo_archive", Path.home() / "upstream_repos" / "repo_archive"):
        try:
            if c.is_dir() and next(c.glob("*/*.git"), None) is not None:
                return str(c.resolve())
        except OSError:
            pass
    return None


RUST_INSTALL_HINT = ("install Rust first: `curl --proto '=https' --tlsv1.2 -sSf "
                     "https://sh.rustup.rs | sh` then restart your shell (see https://rustup.rs)")


def detect_cargo() -> Optional[str]:
    return shutil.which("cargo")


def build_fontc_from_source(dest: Path, on_progress: Optional[Callable[[str], None]] = None) -> str:
    """Clone googlefonts/fontc and `cargo build --release -p fontc`. Returns the binary path."""
    dest = Path(dest)
    binp = dest / "target" / "release" / "fontc"
    if binp.is_file():
        return str(binp)
    if detect_cargo() is None:
        raise RuntimeError("cannot build fontc: cargo (Rust toolchain) not found.\n  " + RUST_INSTALL_HINT)
    if not (dest / ".git").is_dir():
        if on_progress:
            on_progress(f"cloning fontc → {dest}")
        rc, _, err = git(["clone", "--depth", "1", FONTC_URL, str(dest)], timeout=3600)
        if rc != 0:
            tail = err.strip().splitlines()[-1] if err.strip() else str(rc)
            raise RuntimeError(f"fontc clone failed: {tail}")
    if on_progress:
        on_progress("building fontc (cargo build --release -p fontc) — this can take a while…")
    p = subprocess.run(["cargo", "build", "--release", "-p", "fontc"], cwd=str(dest),
                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=7200)
    if p.returncode != 0 or not binp.is_file():
        raise RuntimeError("fontc build failed: " + p.stdout.decode("utf-8", "replace")[-200:])
    return str(binp)


def ensure_google_fonts(path: Path, on_progress: Optional[Callable[[str], None]] = None) -> Path:
    """Clone google/fonts (shallow) if `path` is not already a clone. Returns `path`."""
    if path is None:
        raise ValueError("google/fonts path is required")
    if (path / "ofl").is_dir():
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    if on_progress:
        on_progress(f"cloning google/fonts → {path} (shallow)…")
    rc, _, err = git(["clone", "--depth", "1", GOOGLE_FONTS_URL, str(path)], timeout=3600)
    if rc != 0:
        tail = err.strip().splitlines()[-1] if err.strip() else str(rc)
        raise RuntimeError(f"google/fonts clone failed: {tail}")
    return path


def _repo_short(url: str) -> str:
    """github.com/owner/repo(.git) -> 'owner/repo' for compact live display."""
    u = url.rstrip("/")
    if u.endswith(".git"):
        u = u[:-4]
    parts = [p for p in u.split("/") if p]
    return "/".join(parts[-2:]) if len(parts) >= 2 else (parts[-1] if parts else url)


class KeyedLocks:
    """A registry of per-key locks (here: one per repo_url) so the concurrent archive
    pre-warmer and the build workers never `git clone --mirror` the same repo twice."""
    def __init__(self):
        self._locks: Dict[str, threading.Lock] = {}
        self._guard = threading.Lock()

    def __call__(self, key: str) -> threading.Lock:
        with self._guard:
            lk = self._locks.get(key)
            if lk is None:
                lk = self._locks[key] = threading.Lock()
            return lk


def populate_archive(repo_urls, archive: Path, jobs: int,
                     on_progress: Optional[Callable[[int, int, str, str], None]] = None,
                     stop: "Optional[threading.Event]" = None,
                     clone_lock: "Optional[KeyedLocks]" = None):
    """Ensure every repo_url has a bare mirror in the archive; clone --mirror the missing
    ones (APPEND-ONLY — existing mirrors are skipped read-only and NEVER modified/deleted).
    Returns (added, failed, present). Parallel across `jobs`; aborts promptly if `stop` set.
    `on_progress` fires the moment each clone *completes* (via as_completed) so a slow early
    clone no longer batches the live list. `clone_lock` (shared with the build workers)
    serializes cloning per repo so a repo is never mirrored twice concurrently."""
    from concurrent.futures import ThreadPoolExecutor, as_completed
    urls = sorted(set(repo_urls))
    added: List[str] = []
    failed: List[Tuple[str, str]] = []
    present = 0
    lock = threading.Lock()
    done = [0]

    def one(url: str):
        if stop is not None and stop.is_set():
            return ("skipped", url, "")
        mp = mirror_path(archive, url)
        if mp.is_dir():
            return ("present", url, "")  # existing mirror: read-only, never touched
        lk = clone_lock(url) if clone_lock else None
        if lk:
            lk.acquire()
        try:
            if mp.is_dir():               # another cloner (a worker) won the race — present now
                return ("present", url, "")
            mp.parent.mkdir(parents=True, exist_ok=True)
            rc, _, err = git_clone_mirror(url, str(mp), timeout=1800, stop=stop)
        finally:
            if lk:
                lk.release()
        if rc != 0:
            tail = err.strip().splitlines()[-1] if err.strip() else str(rc)
            return ("failed", url, tail)
        return ("added", url, "")

    with ThreadPoolExecutor(max_workers=max(1, jobs)) as ex:
        futs = [ex.submit(one, u) for u in urls]
        for fut in as_completed(futs):           # report each repo the instant it finishes
            status, url, msg = fut.result()
            with lock:
                done[0] += 1
                if status == "added":
                    added.append(url)
                elif status == "failed":
                    failed.append((url, msg))
                else:
                    present += 1
                if on_progress:
                    on_progress(done[0], len(urls), url, status, msg)
    return added, failed, present


def scan_cohorts(families: List[Family], archive: Path, jobs: int,
                 on_progress: Optional[Callable[[int, int, str], None]] = None,
                 stop: "Optional[threading.Event]" = None):
    """Group families by their normalized repo requirements (read-only `git show` on the
    mirrors). Returns (groups: cohort_key -> [slug], sigs: cohort_key -> requirements).
    Aborts promptly if `stop` is set."""
    from concurrent.futures import ThreadPoolExecutor
    from collections import defaultdict
    groups: Dict[str, List[str]] = defaultdict(list)
    sigs: Dict[str, str] = {}
    lock = threading.Lock()
    done = [0]

    def one(fam: Family):
        if stop is not None and stop.is_set():
            return fam.slug, "(stopped)", ""
        mp = mirror_path(archive, fam.repo_url)
        if not mp.is_dir():
            return fam.slug, "(mirror-absent)", ""
        req = read_requirements_from_mirror(mp, fam.commit)
        return fam.slug, cohort_key_for(req), normalize_requirements(req)

    with ThreadPoolExecutor(max_workers=max(1, jobs)) as ex:
        for slug, cohort, sig in ex.map(one, families):
            with lock:
                groups[cohort].append(slug)
                sigs.setdefault(cohort, sig)
                done[0] += 1
                if on_progress:
                    on_progress(done[0], len(families), slug)
    return dict(groups), sigs


# ============================================================================== config

def resolve_config(google_fonts: Optional[Path], fam: Family, work: Path):
    # google/fonts override has priority (only in metadata mode, where google_fonts is set)
    override = (google_fonts / fam.slug / "config.yaml") if google_fonts is not None else None
    if override is not None and override.is_file():
        dest = work / "__gflib_override_config.yaml"
        try:
            shutil.copyfile(override, dest)
        except OSError as e:
            return None, "", f"could not stage override config: {e}"
        return dest, f"override:{fam.slug}/config.yaml", ""
    if fam.config_yaml:
        p = work / fam.config_yaml
        if p.is_file():
            return p, fam.config_yaml, ""
    for cand in CONFIG_CANDIDATES:
        p = work / cand
        if p.is_file():
            return p, cand, ""
    return None, "", "no config.yaml found (no override, no in-repo config)"


def read_requirements(work: Path) -> str:
    for r in REQ_FILES:
        p = work / r
        if p.is_file():
            try:
                return p.read_text(encoding="utf-8", errors="replace")
            except OSError:
                return ""
    return ""


def normalize_requirements(text: str) -> str:
    lines = []
    for ln in text.splitlines():
        s = ln.split("#", 1)[0].strip()
        if s:
            lines.append(s)
    return "\n".join(sorted(lines))


def cohort_key_for(req_text: str) -> str:
    norm = normalize_requirements(req_text)
    if not norm:
        return "base"
    return "c-" + hashlib.sha1(norm.encode()).hexdigest()[:12]


def read_requirements_from_mirror(mirror: Path, commit: str) -> str:
    """Read a repo's requirements file at a commit WITHOUT extracting the tree — a
    read-only `git show` on the bare mirror (never touches the archive)."""
    for r in REQ_FILES:
        rc, out, _ = git(["--git-dir", str(mirror), "show", f"{commit}:{r}"])
        if rc == 0:
            return out
    return ""


# =============================================================================== venvs

def _req_pkg_name(line: str) -> str:
    """Package name from a requirements line, or '' for blank/comment/option/URL lines."""
    s = line.strip()
    if not s or s.startswith("#") or s.startswith("-") or "://" in s:
        return ""
    m = re.match(r"^([A-Za-z0-9_.][A-Za-z0-9_.\-]*)", s)
    return m.group(1).lower() if m else ""


def _parse_unsatisfiable(text: str) -> set:
    """Packages pip reported it could not satisfy — i.e. a pinned version absent from the index
    (the classic 'No matching distribution found for X==Y' / 'Could not find a version …')."""
    bad = set()
    for pat in (r"Could not find a version that satisfies the requirement\s+([A-Za-z0-9_.\-]+)",
                r"No matching distribution found for\s+([A-Za-z0-9_.\-]+)"):
        for m in re.finditer(pat, text):
            bad.add(m.group(1).lower())
    return bad


# Debian/Ubuntu -dev packages for the native libraries the font-build Python stack compiles against
# (only relevant when pip has to build a package from source — e.g. on a Python with no wheels).
_SYSLIB_HINT = {"cairo": "libcairo2-dev", "freetype2": "libfreetype-dev", "freetype": "libfreetype-dev",
                "fontconfig": "libfontconfig1-dev", "harfbuzz": "libharfbuzz-dev",
                "glib-2.0": "libglib2.0-dev", "libffi": "libffi-dev"}


def scan_missing_system_dep(log_text: str):
    """If a pip install failed building a C/meson extension because a SYSTEM library is missing
    (not a Python-pin problem the self-heal can fix), return a short '<lib> (install <pkg>)' string,
    else None. Recognises meson's `Dependency "X" not found`, pkg-config's `No package 'X' found`,
    and gcc's `fatal error: X.h: No such file`."""
    for rx in (r'Dependency "([^"]+)" not found', r"No package '([^']+)' found"):
        m = re.search(rx, log_text)
        if m:
            lib = m.group(1)
            pkg = _SYSLIB_HINT.get(lib.lower())
            return f"{lib} ({'install ' + pkg if pkg else 'install its -dev package'})"
    m = re.search(r"fatal error:\s*([\w./+-]+\.h):\s*No such file", log_text)
    if m:
        return f"C headers <{m.group(1)}> (install the matching -dev package)"
    return None


def relax_requirements(lines: List[str], relax: set) -> List[str]:
    """Drop the version pin (keep just the package name) for any requirement whose package is in
    `relax`, so pip's resolver backtracks to a compatible version instead of failing on an
    absent/dev pin. Other pins are untouched, so reproducibility holds for everything valid."""
    out = []
    for ln in lines:
        pkg = _req_pkg_name(ln)
        if pkg and pkg in relax:
            out.append(f"{pkg}    # auto-relaxed by gflib-build: pinned version unavailable on PyPI")
        else:
            out.append(ln)
    return out


class VenvManager:
    """Create and reuse one venv per distinct dependency cohort.

    Families with no/standard requirements share the `base` cohort venv; families whose
    repo requirements.txt is identical share a cohort venv keyed by its content hash.
    """

    def __init__(self, build_dir: Path, base_python: str, base_requirements: Optional[Path]):
        self.root = build_dir / "venvs"
        self.pip_cache = build_dir / "pip-cache"
        self.base_python = base_python
        self.base_req = base_requirements
        self.root.mkdir(parents=True, exist_ok=True)
        self.pip_cache.mkdir(parents=True, exist_ok=True)
        self._global = threading.Lock()
        self._locks: Dict[str, threading.Lock] = {}
        self._ready: Dict[str, str] = {}
        self._relaxed: set = set()               # base pins auto-relaxed once, shared by cohorts
        self.relaxations: List[str] = []         # human-readable record (surfaced to the UI)

    def cohort_key(self, req_text: str) -> str:
        return cohort_key_for(req_text)

    def _lock_for(self, key: str) -> threading.Lock:
        with self._global:
            return self._locks.setdefault(key, threading.Lock())

    def ensure_base(self) -> str:
        py, err = self._create("base", "")
        if err:
            raise RuntimeError(f"base venv creation failed: {err}")
        with self._global:
            self._ready["base"] = py
        return py

    def get_python(self, req_text: str, on_install: Optional[Callable[[str], None]] = None):
        key = self.cohort_key(req_text)
        with self._global:
            if key in self._ready:
                return self._ready[key], key, ""
        with self._lock_for(key):
            with self._global:
                if key in self._ready:
                    return self._ready[key], key, ""
            if on_install:
                on_install(key)
            py, err = self._create(key, req_text)
            if not err:
                with self._global:
                    self._ready[key] = py
            return py, key, err

    def ready_count(self) -> int:
        with self._global:
            return len(self._ready)

    def _create(self, key: str, req_text: str):
        vdir = self.root / key
        py = vdir / "bin" / "python"
        ready = vdir / ".gflib-installed"        # written ONLY after a successful pip install
        log = self.root / f"{key}.install.log"
        if ready.exists() and py.exists():       # verified-ready: reuse it
            return str(py), ""
        # No success marker → brand-new, OR a previous install failed (e.g. an unavailable pin like
        # the old gftools==0.9.100.dev4) and left a venv with only pip. Never resurrect that broken
        # shell — rebuild from scratch so the (now-fixable) requirements get re-installed.
        if vdir.exists():
            shutil.rmtree(vdir, ignore_errors=True)
        rc = subprocess.run([self.base_python, "-m", "venv", str(vdir)],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if rc.returncode != 0:
            return "", f"venv create rc={rc.returncode}: {rc.stdout.decode('utf-8','replace')[:200]}"

        base_lines = (self.base_req.read_text().splitlines()
                      if (self.base_req and self.base_req.is_file()) else [])
        cohort_lines = req_text.splitlines() if key != "base" else []
        base_pkgs = {_req_pkg_name(l) for l in base_lines} - {""}
        eff_path = vdir / "effective-requirements.txt"
        with self._global:
            relax = set(self._relaxed)            # start from base pins already known-broken
        # SELF-HEALING install: if pip can't satisfy a pinned version (a stale/dev pin absent
        # from PyPI), drop just that pin and retry — so the user never has to hand-manage pins.
        for attempt in range(6):
            eff = relax_requirements(base_lines + cohort_lines, relax)
            eff_path.write_text("\n".join(eff) + "\n")
            install = [str(py), "-m", "pip", "install", "--disable-pip-version-check",
                       "--cache-dir", str(self.pip_cache), "-r", str(eff_path)]
            with open(log, "wb" if attempt == 0 else "ab") as lf:
                if relax:
                    lf.write(f"# gflib-build attempt {attempt + 1}: auto-relaxed pins "
                             f"{sorted(relax)}\n".encode())
                p = subprocess.run(install, stdout=lf, stderr=subprocess.STDOUT)
            if p.returncode == 0:
                try:
                    ready.write_text("ok\n")      # mark verified-ready (else we'd reinstall next run)
                except OSError:
                    pass
                base_fixed = relax & base_pkgs
                if base_fixed:
                    with self._global:
                        new = base_fixed - self._relaxed
                        self._relaxed |= base_fixed
                        if new:                   # record once, for the UI / log
                            self.relaxations.append(
                                f"auto-relaxed base pins (unavailable on PyPI): {sorted(new)}")
                return str(py), ""
            log_text = log.read_text(errors="replace")
            bad = _parse_unsatisfiable(log_text)
            if not (bad - relax):                 # nothing NEW to relax → a genuine failure
                syslib = scan_missing_system_dep(log_text)   # a missing C library, not a pin we can fix
                if syslib:
                    return "", f"missing system library: {syslib} (see {log.name})"
                note = f" after auto-relaxing {sorted(relax)}" if relax else ""
                return "", f"pip install rc={p.returncode}{note} (see {log.name})"
            relax |= bad
        return "", f"pip install failed even after auto-relaxing {sorted(relax)} (see {log.name})"


# ============================================================================= building

def run_builder(python: str, config_path: Path, work: Path, log_path: Path,
                timeout: Optional[int], backend: str, fontc_bin: Optional[str]):
    """`timeout=None` means the build never times out (the user can stop it via the UI)."""
    env = dict(os.environ)
    env["SOURCE_DATE_EPOCH"] = "0"
    # gftools.builder shells out to fontmake / ninja / gftools / ttfautohint BY NAME, so
    # the chosen interpreter's bin/ must be on PATH (running venv/bin/python does not, by
    # itself, activate the venv).
    # ABSOLUTE interpreter path: the builder subprocess runs with cwd=work (the extraction
    # dir), so a relative python path (e.g. from a relative --data-dir) would resolve against
    # work and fail with "could not launch builder: No such file or directory".
    python = os.path.abspath(python)
    bindir = os.path.dirname(python)
    env["PATH"] = bindir + os.pathsep + env.get("PATH", "")
    cmd = [python, "-m", "gftools.builder", str(config_path)]
    if backend == "fontc":
        cmd += ["--experimental-fontc", fontc_bin]
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with open(log_path, "ab") as logf:   # append: the whole pipeline shares one family log
        logf.write(f"\n===== gftools.builder (backend={backend}) =====\n"
                   f"# {' '.join(cmd)}\n# cwd={work}\n\n".encode())
        logf.flush()
        try:
            p = subprocess.run(cmd, cwd=str(work), env=env,
                               stdout=logf, stderr=subprocess.STDOUT, timeout=timeout)
        except subprocess.TimeoutExpired:
            return False, f"{backend}: timed out after {timeout}s"
        except OSError as e:
            return False, f"{backend}: could not launch builder: {e}"
    if p.returncode != 0:
        return False, f"{backend}: " + (_last_error_line(log_path) or f"exit {p.returncode}")
    return True, ""


def _last_error_line(log_path: Path) -> str:
    try:
        lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return ""
    for ln in reversed(lines):
        s = ln.strip()
        if s and any(k in s for k in ("Error", "error", "Exception", "Traceback", "FAILED", "assert")):
            return s[:200]
    return lines[-1].strip()[:200] if lines else ""


def collect_outputs(work: Path, out_dir: Path, shipped: List[str]):
    found: Dict[str, Path] = {}
    out_dir.mkdir(parents=True, exist_ok=True)
    total = 0
    want = set(shipped)
    for sub in FONT_SUBDIRS:
        d = work / sub
        if not d.is_dir():
            continue
        for f in sorted(d.iterdir()):
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
    h = hashlib.sha256()
    try:
        with open(path, "rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                h.update(chunk)
    except OSError:
        return ""
    return h.hexdigest()


def diff_font_tables(python: str, a: Path, b: Path) -> List[str]:
    """Return the OpenType table tags that differ between two fonts (raw compiled bytes),
    using fontTools in the build interpreter. ['?'] if the comparison itself failed."""
    script = (
        "import sys\n"
        "from fontTools.ttLib import TTFont\n"
        "fa=TTFont(sys.argv[1]); fb=TTFont(sys.argv[2])\n"
        "ka=set(fa.keys()); kb=set(fb.keys())\n"
        "d=[t for t in (ka|kb) if t!='GlyphOrder' and "
        "(fa.getTableData(t) if t in ka else None)!=(fb.getTableData(t) if t in kb else None)]\n"
        "print(','.join(sorted(d)))\n")
    try:
        p = subprocess.run([python, "-c", script, str(a), str(b)],
                           capture_output=True, text=True, timeout=180)
        if p.returncode != 0:
            return ["?"]
        return [t for t in p.stdout.strip().split(",") if t]
    except Exception:
        return ["?"]


def compare_backends(python: str, fontc_built: Dict[str, Path], fontmake_built: Dict[str, Path],
                     shipped: List[str]) -> str:
    """Compare fontc vs fontmake outputs (fontc_crater-style). Returns 'identical',
    'differ:<tables>', or '' if there were no comparable pairs."""
    names = shipped or sorted(set(fontc_built) & set(fontmake_built))
    tags, any_pair = set(), False
    for fn in names:
        a, b = fontc_built.get(fn), fontmake_built.get(fn)
        if a is None or b is None:
            continue
        any_pair = True
        if sha256(a) != sha256(b):
            tags.update(diff_font_tables(python, a, b) or ["?"])
    if not any_pair:
        return ""
    if not tags:
        return "identical"
    return "differ:" + ",".join(sorted(tags)[:6])


def compare_to_shipped(google_fonts: Path, fam: Family, built: Dict[str, Path]) -> str:
    if not fam.shipped_fonts:
        return ""
    fam_dir = google_fonts / fam.slug
    all_identical, any_present = True, False
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
    return ("identical" if all_identical else "differ") if any_present else "missing"


# ========================================================================= orchestrator

class Orchestrator:
    """UI-agnostic build core. Exposes snapshot(); writes state.json + events.jsonl."""

    def __init__(self, args):
        self.args = args
        self.build_dir = Path(args.build_dir)
        self.google_fonts = Path(args.google_fonts) if args.google_fonts else None
        self.archive = Path(args.archive)
        # the worklist is discovered live, inside the driver (a task), so it shows in the UI
        self.families: Dict[str, Family] = {}
        self._all_families: List[Family] = []   # full discovered list (for live percent changes)
        self.total_with_source = 0
        self.skipped_no_config = 0
        self.control_log: List[str] = []        # live config changes applied (for the UI)
        self._control_last = 0                   # last applied control.json seq

        self.lock = threading.Lock()
        self.results: Dict[str, Result] = {}
        self.q: "queue.Queue[str]" = queue.Queue()
        self.stop = threading.Event()
        self.paused = threading.Event()
        self.start_time = time.time()
        self._resumed_elapsed = 0.0   # active wall-time from prior sessions (so the clock
                                       # is cumulative across reopen/resume, not reset to 0)
        self.disk_baseline = self._disk_used()
        self._build_total = 0                        # build-dir total bytes; refreshed off-thread
        self._enqueued = 0                            # families queued this run; how many are retries
        self._enqueued_retries = 0
        self.failures: List[str] = []
        self.workers: List[threading.Thread] = []
        self._wid_counter = 0                    # monotonic worker ids (no collisions on respawn)
        self._events = open(self.build_dir / "events.jsonl", "a", buffering=1)
        self._events_lock = threading.Lock()
        self._events_closed = False

        self.venvs: Optional[VenvManager] = None
        if args.manage_venvs:
            self.venvs = VenvManager(self.build_dir, args.base_python,
                                     Path(args.base_requirements) if args.base_requirements else None)

        # phase pipeline state (clone_gf → build_fontc → discover → archive → cohorts → build → done)
        self.phase = "init"
        self.phase_total = 0
        self.phase_done = 0
        self.phase_label = ""
        self.phase_error = ""
        self.cohorts: Dict[str, dict] = {}
        self.archive_log: List[Tuple[str, str, str]] = []  # (status, owner/repo, reason) per mirror
        self._archive_seen: set = set()                # de-dup repos across pre-warmer + workers
        self._cohort_members: Dict[str, set] = {}      # cohort key -> {slugs} (assigned live)
        self._cohort_reqs: Dict[str, str] = {}         # cohort key -> normalized requirements
        self.clone_locks = KeyedLocks()                # per-repo: pre-warmer vs workers never clash
        self.driver: Optional[threading.Thread] = None
        # end-to-end task-list rendered live in the UI (each maps to a phase key)
        self.tasks: List[Task] = self._build_tasks()
        # timing instrumentation (every operation is measured, for bottleneck analysis)
        self.op_stats: Dict[str, List[float]] = {}   # op -> [total_seconds, count, max]
        self.phase_durations: Dict[str, float] = {}
        self._phase_t0: Optional[float] = None
        self._status_thread: Optional[threading.Thread] = None
        self._control_thread: Optional[threading.Thread] = None
        self._status_stop = threading.Event()
        self._status_lock = threading.Lock()

        self._load_state()   # _enqueue() happens later, in the discover task of the driver

    # ---- task-list (end-to-end pipeline shown live in the UI)
    def _build_tasks(self) -> List[Task]:
        gf = self.google_fonts
        need_clone = (self.args.source == "metadata"
                      and not (gf and (gf / "ofl").is_dir()))
        want_fontc = bool(getattr(self.args, "_want_build_fontc", False))
        populate = bool(getattr(self.args, "populate_archive", False))
        return [
            Task("clone_gf", "clone google/fonts",
                 "pending" if need_clone else "skipped"),
            Task("build_fontc", "build fontc from source",
                 "pending" if want_fontc else "skipped"),
            Task("discover", "discover worklist (METADATA / archive)"),
            # archive (pre-warm) and build run CONCURRENTLY; cohorts are assigned live by the
            # workers as each repo becomes available (no separate scan barrier).
            Task("archive", "populate archive (mirror missing)",
                 "pending" if populate else "skipped"),
            Task("build", "build fonts (mirror + cohort + compile, streaming)"),
        ]

    def _task(self, key: str) -> Optional[Task]:
        for t in self.tasks:
            if t.key == key:
                return t
        return None

    def _task_start(self, key: str, total: int = 0, detail: str = ""):
        with self.lock:
            t = self._task(key)
            if t is not None:
                t.status, t.t0, t.t1, t.total, t.done, t.detail = (
                    "running", time.time(), 0.0, total, 0, detail)
        self._begin_phase(key, total)            # drives the phase banner + per-phase timing

    def _task_progress(self, key: str, done: int, detail: str = ""):
        with self.lock:
            t = self._task(key)
            if t is not None:
                t.done, t.detail = done, detail
        self._phase_progress(done, detail)

    def _task_done(self, key: str, detail: str = ""):
        with self.lock:
            t = self._task(key)
            if t is not None:
                t.status, t.t1 = "done", time.time()
                if detail:
                    t.detail = detail

    def _task_fail(self, key: str, detail: str = ""):
        with self.lock:
            t = self._task(key)
            if t is not None:
                t.status, t.t1, t.detail = "failed", time.time(), detail
            self.phase_error = detail

    # ---- concurrent-task helpers: update a task WITHOUT grabbing the global phase (so the
    # archive pre-warmer and the build can both show "running" at once; the build owns `phase`)
    def _task_running(self, key: str, total: int = 0, detail: str = ""):
        with self.lock:
            t = self._task(key)
            if t is not None:
                t.status, t.t0, t.t1, t.total, t.done, t.detail = (
                    "running", time.time(), 0.0, total, 0, detail)

    def _task_update(self, key: str, done: int, detail: str = ""):
        with self.lock:
            t = self._task(key)
            if t is not None:
                t.done, t.detail = done, detail

    def _note_mirrored(self, url: str, status: str, reason: str = ""):
        """Record a repo the instant it lands in the archive (pre-warmer OR a worker), de-duped,
        so the live archive list grows gradually as mirroring happens. `reason` is the git error
        for a 'failed' mirror (shown in the status panel so the red entries are explained)."""
        short = _repo_short(url)
        with self.lock:
            if short in self._archive_seen:
                return
            self._archive_seen.add(short)
            self.archive_log.append((status, short, reason))
            if len(self.archive_log) > 400:
                del self.archive_log[:-400]

    def _note_cohort(self, slug: str, cohort: str, req_text: str):
        """Assign a family to its cohort live, as soon as its repo is available — rebuilds the
        cohorts view incrementally (no global scan barrier)."""
        with self.lock:
            self._cohort_members.setdefault(cohort, set()).add(slug)
            self._cohort_reqs.setdefault(cohort, normalize_requirements(req_text))
            self.cohorts = {k: {"count": len(v), "requirements": self._cohort_reqs.get(k, ""),
                                "families": sorted(self.families[s].name if s in self.families
                                                   else s for s in v)}
                            for k, v in sorted(self._cohort_members.items(),
                                               key=lambda kv: -len(kv[1]))}

    def _archive_progress(self, done: int, url: str, status: str, reason: str = ""):
        """Per-repo callback from the concurrent archive pre-warmer: feed the live list and
        advance the (concurrent) archive task — WITHOUT touching `phase` (the build owns it)."""
        if status in ("added", "failed"):
            self._note_mirrored(url, status, reason)
        self._task_update("archive", done, f"{status}: {_repo_short(url)}")

    # ---- phase helpers (also accumulate per-phase wall-clock durations)
    def _close_phase(self, now: float):
        if self.phase not in ("init", "done") and self._phase_t0 is not None:
            self.phase_durations[self.phase] = (self.phase_durations.get(self.phase, 0.0)
                                                + (now - self._phase_t0))

    def _begin_phase(self, name: str, total: int):
        now = time.time()
        with self.lock:
            self._close_phase(now)
            self.phase, self.phase_total, self.phase_done, self.phase_label = name, total, 0, ""
            self._phase_t0 = now

    def _phase_progress(self, done: int, label: str = ""):
        with self.lock:
            self.phase_done, self.phase_label = done, label

    def _set_phase(self, name: str):
        now = time.time()
        with self.lock:
            self._close_phase(now)
            self.phase = name
            self._phase_t0 = now

    def _record_op(self, slug: str, op: str, dt: float):
        with self.lock:
            s = self.op_stats.setdefault(op, [0.0, 0, 0.0])
            s[0] += dt; s[1] += 1; s[2] = max(s[2], dt)
            r = self.results.get(slug)
            if r is not None:
                r.timings[op] = r.timings.get(op, 0.0) + dt

    # ---- persistence / events
    @property
    def state_path(self) -> Path:
        return self.build_dir / "state.json"

    def _load_state(self):
        if self.state_path.is_file():
            try:
                data = json.loads(self.state_path.read_text())
                for slug, r in data.get("results", {}).items():
                    self.results[slug] = Result(**{k: v for k, v in r.items()
                                                   if k in Result.__dataclass_fields__})
                # carry the prior sessions' elapsed so the clock continues, not resets
                self._resumed_elapsed = float(data.get("elapsed_so_far", 0.0))
            except Exception:
                pass

    def save_state(self):
        with self.lock:
            data = {"saved_at": time.time(), "build_dir": str(self.build_dir),
                    "elapsed_so_far": self._resumed_elapsed + (time.time() - self.start_time),
                    "results": {s: asdict(r) for s, r in self.results.items()}}
        tmp = self.state_path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(data, indent=1))
        tmp.replace(self.state_path)

    def _emit(self, etype: str, slug: str, **extra):
        ev = {"t": round(time.time() - self.start_time, 2), "type": etype, "slug": slug}
        ev.update(extra)
        try:
            with self._events_lock:
                if self._events_closed:
                    return
                self._events.write(json.dumps(ev) + "\n")
        except Exception:
            pass

    # ---- scheduling
    def _enqueue(self):
        only = set(self.args.only.split(",")) if self.args.only else None
        todo, retries = [], 0
        for slug, fam in self.families.items():
            if only and slug not in only:
                continue
            prev = self.results.get(slug)
            if prev and not self.args.rebuild:
                if prev.status == "built":
                    continue
                if prev.status == "failed":
                    # Self-heal: re-attempt a failure whose cause a fresh try can clear (rebuilt
                    # venv, retried clone, …). Keep genuine build errors / unreachable repos unless
                    # the user forces it with retry_failed. This is what makes the failure hints
                    # honest — the family is actually retried, not silently skipped.
                    cat, _ = categorize_failure(prev.error or "")
                    if not (self.args.retry_failed or cat in AUTO_RETRY_CATEGORIES):
                        continue
                    retries += 1
            todo.append(slug)
        self._enqueued = len(todo)
        self._enqueued_retries = retries

        def weight(slug):
            prev = self.results.get(slug)
            if prev and prev.ended > prev.started:        # resume: longest prior build first
                return prev.dur()
            fam = self.families[slug]                     # first run: heuristic
            return (1000 if fam.is_variable else 0) + len(fam.shipped_fonts)
        todo.sort(key=weight, reverse=True)

        # Reconcile stale in-flight results from a PRIOR run that aren't in this run's worklist
        # (e.g. families queued at a higher --percent, now outside the sample, or an interrupted
        # build). Left as "queued"/"building" they'd show as perpetually pending though nothing
        # will ever process them. Mark them skipped so the counts match the actual worklist; they
        # re-enter naturally if the selection later includes them (skipped != built/failed).
        todo_set = set(todo)
        with self.lock:
            for slug, r in self.results.items():
                if slug not in todo_set and r.status in ("queued", "building"):
                    r.status, r.note = "skipped", "not selected this run"

        for slug in todo:
            with self.lock:    # _enqueue now runs in the driver thread, racing the status writer
                self.results[slug] = Result(slug=slug, status="queued")
            self.q.put(slug)

    # ---- read-only views
    def _disk_used(self) -> int:
        try:
            return shutil.disk_usage(self.build_dir).used
        except OSError:
            return 0

    def _measure_build_dir(self) -> int:
        """Compute the total bytes the build system occupies on disk (the whole build dir: out/,
        venvs/, work/, caches, logs). SLOW (a full tree walk) — must run only on the background
        size thread, never on a render/snapshot path. `du`'s reported total is valid even when it
        exits non-zero because a file vanished mid-walk (workers churn work/ during a build), so we
        trust any numeric stdout and only fall back to os.walk when du gives nothing usable."""
        try:                                          # `du` is fast (C); falls back to os.walk
            p = subprocess.run(["du", "-sk", str(self.build_dir)],
                               capture_output=True, text=True, timeout=60)
            tok = p.stdout.split()
            if tok and tok[0].isdigit():
                return int(tok[0]) * 1024             # partial-but-fine even if returncode != 0
        except Exception:
            pass
        total = 0
        try:
            for root, _dirs, files in os.walk(self.build_dir):
                for f in files:
                    try:
                        total += os.path.getsize(os.path.join(root, f))
                    except OSError:
                        pass
        except Exception:
            pass
        return total

    def _size_writer(self):
        """Refresh self._build_total off the UI thread, so snapshot() (called every ~0.25 s on the
        curses render thread and every ~1 s on the status writer) only ever reads a plain int."""
        while not self.stop.is_set():
            try:
                self._build_total = self._measure_build_dir()
            except Exception:
                pass
            self.stop.wait(10)                        # interruptible ~10 s pacing

    def snapshot(self) -> dict:
        with self.lock:
            rs = list(self.results.values())
            counts = {"built": 0, "failed": 0, "building": 0, "queued": 0, "skipped": 0}
            backends = {"fontc": 0, "fontmake": 0}
            migration = {"fontc": 0, "fontmake_fallback": 0, "fontmake_only": 0,
                         "both_identical": 0, "both_differ": 0}
            fail_cat = {}                            # cause -> [count, hint] for the failure summary
            building = []
            for r in rs:
                counts[r.status] = counts.get(r.status, 0) + 1
                if r.status == "failed":
                    cat, hint = categorize_failure(r.error)
                    slot = fail_cat.setdefault(cat, [0, hint]); slot[0] += 1
                if r.status == "built" and r.backend:
                    backends[r.backend] = backends.get(r.backend, 0) + 1
                    if r.backend == "fontc":
                        migration["fontc"] += 1
                    elif r.backend == "fontmake":
                        migration["fontmake_fallback" if r.fontc_error else "fontmake_only"] += 1
                    elif r.backend == "both":
                        if r.fontc_ok and r.fontmake_ok:
                            migration["both_identical" if r.vs == "identical" else "both_differ"] += 1
                        elif r.fontc_ok:                 # fontc built, fontmake didn't
                            migration["fontc"] += 1
                        else:                            # fontc failed (blocker)
                            migration["fontmake_fallback"] += 1
                if r.status == "building":
                    building.append({"slug": r.slug, "worker": r.worker, "dur": r.dur(),
                                     "backend": r.backend, "note": r.note})
            building.sort(key=lambda b: -b["dur"])
            fail_categories = sorted(
                [{"cat": k, "count": v[0], "hint": v[1]} for k, v in fail_cat.items()],
                key=lambda c: -c["count"])
            fails = [{"slug": s, "error": self.results[s].error, "log": self.results[s].log}
                     for s in self.failures[-50:] if s in self.results][::-1]
            built = sorted(([{"slug": r.slug, "backend": r.backend, "bytes": r.out_bytes,
                              "compare": r.compare, "log": r.log, "ended": r.ended}
                             for r in rs if r.status == "built"]),
                           key=lambda b: -b["ended"])[:200]
        try:
            du = shutil.disk_usage(self.build_dir)
            disk_delta, disk_free = max(0, du.used - self.disk_baseline), du.free
        except OSError:
            disk_delta, disk_free = 0, 0
        with self.lock:
            phase, ptot, pdone, plabel, perr = (self.phase, self.phase_total,
                                                self.phase_done, self.phase_label, self.phase_error)
            cohorts = list(self.cohorts.items())
            op_stats = {op: {"total": round(s[0], 2), "count": s[1],
                             "mean": round(s[0] / s[1], 3) if s[1] else 0.0, "max": round(s[2], 2)}
                        for op, s in self.op_stats.items()}
            phase_dur = {k: round(v, 1) for k, v in self.phase_durations.items()}
            tasks = [{"key": t.key, "name": t.name, "status": t.status,
                      "elapsed": round(t.elapsed(), 1), "done": t.done,
                      "total": t.total, "detail": t.detail} for t in self.tasks]
            archive_recent = [{"status": s, "repo": r, "reason": (rs[0] if rs else "")}
                              for s, r, *rs in self.archive_log[-60:]]
            control_log = list(self.control_log[-12:])
            config = {                               # built under the lock (apply_live mutates args)
                "source": self.args.source, "google_fonts": self.args.google_fonts,
                "archive": str(self.archive), "build_dir": str(self.build_dir),
                "backend": self.args.backend, "fontc_bin": self.args.fontc_bin,
                "jobs": self.args.jobs, "percent": self.args.percent,
                "timeout": self.args.timeout, "populate_archive": bool(self.args.populate_archive),
                "manage_venvs": bool(self.args.manage_venvs), "compare": bool(self.args.compare),
                "retry_failed": bool(self.args.retry_failed), "only": self.args.only,
            }
        return {
            "elapsed": self._resumed_elapsed + (time.time() - self.start_time),
            "disk_used_delta": disk_delta, "disk_free": disk_free,
            "disk_build_total": self._build_total,   # plain int, refreshed by _size_writer (off-thread)
            "jobs": self.args.jobs, "paused": self.paused.is_set(),
            "total": len(rs), "counts": counts, "backends": backends,
            "building": building, "failures_recent": fails, "built_recent": built,
            "fail_categories": fail_categories,
            "cohorts_ready": self.venvs.ready_count() if self.venvs else 0,
            "phase": phase, "phase_total": ptot, "phase_done": pdone,
            "phase_label": plabel, "phase_error": perr,
            "cohorts": [{"key": k, "count": v["count"], "requirements": v["requirements"],
                         "families": v.get("families", [])} for k, v in cohorts],
            "op_stats": op_stats, "phase_durations": phase_dur, "migration": migration,
            "tasks": tasks, "archive_recent": archive_recent, "config": config,
            "control_log": control_log,
            "config_path": getattr(self.args, "_cfg_path", ""),
            "dep_relaxations": list(self.venvs.relaxations) if self.venvs else [],
            "done": phase == "done",
        }

    def all_done(self) -> bool:
        with self.lock:
            return bool(self.results) and all(
                r.status in ("built", "failed", "skipped") for r in self.results.values())

    # ---- workers
    def _set(self, slug: str, **kw):
        with self.lock:
            r = self.results[slug]
            for k, v in kw.items():
                setattr(r, k, v)

    def worker(self, wid: int):
        while not self.stop.is_set():
            if self.paused.is_set():
                time.sleep(0.2)
                continue
            try:
                slug = self.q.get(timeout=0.3)
            except queue.Empty:
                if self.all_done() and self.q.empty():
                    return
                continue
            if self.stop.is_set():
                self.q.task_done()
                return
            try:
                self._build_one(wid, slug)
            except Exception as e:  # never let a worker die silently
                self._fail(slug, f"harness error: {e}")
            finally:
                self.q.task_done()

    def _fail(self, slug: str, msg: str):
        # NOTE: the throwaway work/ extraction is cleaned by _build_one's finally; here we
        # only drop any partial collected outputs so failures never leak disk under out/.
        self._set(slug, status="failed", ended=time.time(), error=msg, note="")
        with self.lock:
            if slug not in self.failures:
                self.failures.append(slug)
        self._emit("failed", slug, error=msg)
        shutil.rmtree(self.build_dir / "out" / slug.replace("/", "__"), ignore_errors=True)
        self.save_state()

    def _build_one(self, wid: int, slug: str):
        fam = self.families[slug]
        safe = slug.replace("/", "__")
        work = self.build_dir / "work" / safe
        out_dir = self.build_dir / "out" / safe
        log_rel = f"logs/{safe}.log"
        log_path = self.build_dir / log_rel
        t_start = time.time()
        try:                                            # comprehensive per-family log (kept always)
            log_path.write_text(f"# {slug}\n# repo={fam.repo_url}\n# commit={fam.commit}\n")
        except OSError:
            pass

        def flog(msg):
            try:
                with open(log_path, "a") as lf:
                    lf.write(f"[+{time.time() - t_start:6.1f}s] {msg}\n")
            except OSError:
                pass

        def timed(op, fn):                              # measure every operation
            t0 = time.time()
            r = fn()
            self._record_op(slug, op, time.time() - t0)
            return r

        # note="checkout" from the very start so the family's name + step is visible in the
        # "Now building" panel while its checkout (mirror + extract) is still happening
        self._set(slug, status="building", started=time.time(), worker=wid,
                  ended=0.0, error="", note="checkout", backend="", log=log_rel)
        self._emit("started", slug, worker=wid)
        try:
            # clone-on-demand whenever we're populating the archive (or --mirror-missing): the
            # worker may reach a family before the pre-warmer mirrored its repo. The shared
            # per-repo lock means only one of them clones; on_clone feeds the live archive list.
            clone_ok = self.args.mirror_missing or bool(self.args.populate_archive)
            mirror, err = timed("mirror", lambda: ensure_mirror(
                self.archive, fam.repo_url, fam.commit, clone_ok,
                clone_lock=self.clone_locks,
                on_clone=lambda u: self._note_mirrored(u, "added"), stop=self.stop))
            flog("mirror: " + (f"ok ({mirror.name})" if not err else f"FAIL {err}"))
            if err:
                return self._fail(slug, err)
            err = timed("extract", lambda: extract_tree(mirror, fam.commit, work, EXTRACT_TIMEOUT))
            flog("extract: " + ("ok" if not err else f"FAIL {err}"))
            if err:
                return self._fail(slug, err)
            self._set(slug, note="")           # checked out — next step sets its own tag

            if self.venvs is not None:
                req = read_requirements(work)

                def installing(key):
                    self._set(slug, note=f"installing deps ({key})")
                    self._emit("venv", slug, cohort=key)
                    flog(f"venv: installing cohort {key}…")
                python, cohort, verr = timed("venv", lambda: self.venvs.get_python(req, installing))
                self._set(slug, cohort=cohort, note="")
                self._note_cohort(slug, cohort, req)      # live cohort assignment → cohorts view
                flog(f"venv: cohort {cohort} " + ("ok" if not verr else f"FAIL {verr}"))
                if verr:
                    return self._fail(slug, f"venv: {verr}")
            else:
                python = self.args.build_python

            def attempt(b: str, dest: Path, fresh: bool):
                """Build with one backend into `dest`; returns (ok, err, built_dict, bytes)."""
                if fresh:
                    e = timed("extract", lambda: extract_tree(mirror, fam.commit, work, EXTRACT_TIMEOUT))
                    if e:
                        return False, e, {}, 0
                preclean_outputs(work)
                cfg, label, cerr = timed("config", lambda: resolve_config(self.google_fonts, fam, work))
                if cerr:
                    return False, cerr, {}, 0
                self._set(slug, backend=b, config_used=label)
                flog(f"build[{b}]: config={label} — running gftools.builder…")
                t0 = time.time()
                bok, berr = run_builder(python, cfg, work, log_path, self.args.timeout, b, self.args.fontc_bin)
                self._record_op(slug, "build", time.time() - t0)
                flog(f"build[{b}]: " + ("OK" if bok else f"FAIL {berr}") + f"  ({time.time() - t0:.0f}s)")
                if not bok:
                    return False, berr, {}, 0
                nb, bd = collect_outputs(work, dest, fam.shipped_fonts)
                return True, "", bd, nb

            if self.args.backend == "both":
                # fontc_crater-style: build with BOTH compilers and compare their outputs
                fok, ferr, fbuilt, fbytes = attempt("fontc", out_dir / "fontc", fresh=False)
                mok, merr, mbuilt, mbytes = attempt("fontmake", out_dir / "fontmake", fresh=True)
                if not (fok or mok):
                    self._set(slug, fontc_error=ferr)
                    # keep BOTH errors (the categoriser keys on substrings like 'No module named
                    # gftools'; dropping fontmake's error or over-truncating would misclassify a
                    # broken-venv failure as a non-retryable build error)
                    return self._fail(slug, f"both backends failed — fontc: {ferr[:120]} || "
                                            f"fontmake: {merr[:120]}")
                vs = ""
                if fok and mok:
                    vs = timed("vs", lambda: compare_backends(python, fbuilt, mbuilt, fam.shipped_fonts))
                flog(f"DONE both: fontc={'ok' if fok else 'FAIL'} fontmake={'ok' if mok else 'FAIL'} vs={vs or '-'}")
                self._set(slug, status="built", ended=time.time(), backend="both", note="",
                          out_bytes=(fbytes if fok else mbytes), fontc_ok=fok, fontmake_ok=mok,
                          vs=vs, fontc_error=("" if fok else ferr))
                self._emit("built", slug, backend="both", fontc_ok=fok, fontmake_ok=mok, vs=vs,
                           dur=round(self.results[slug].dur(), 1))
            else:
                order = self._backend_order()
                ok, berr, used, fontc_err = False, "", "", ""
                for i, b in enumerate(order):
                    ok, berr, built, nbytes = attempt(b, out_dir, fresh=(i > 0))
                    if ok:
                        used = b
                        break
                    if b == "fontc":
                        fontc_err = berr            # fontc couldn't build this — a migration blocker
                if not ok:
                    self._set(slug, fontc_error=fontc_err)
                    return self._fail(slug, berr or "build failed")
                if fam.shipped_fonts and not built:
                    flog("collect: FAIL produced no expected font files")
                    self._set(slug, fontc_error=fontc_err)
                    return self._fail(slug, f"{used}: produced no expected font files")
                missing = [f for f in fam.shipped_fonts if f not in built]
                cmp_label = ""
                if self.args.compare:
                    cmp_label = timed("compare", lambda: compare_to_shipped(self.google_fonts, fam, built))
                flog(f"DONE: backend={used} bytes={nbytes} missing={len(missing)} compare={cmp_label or '-'}"
                     + (f"  (fontc fell back: {fontc_err[:60]})" if used == "fontmake" and fontc_err else ""))
                self._set(slug, status="built", ended=time.time(), out_bytes=nbytes,
                          out_missing=len(missing), compare=cmp_label, backend=used, note="",
                          fontc_error=fontc_err)
                self._emit("built", slug, backend=used, bytes=nbytes, compare=cmp_label,
                           missing=len(missing), fontc_failed=bool(fontc_err),
                           dur=round(self.results[slug].dur(), 1))
            if not self.args.keep_fonts:
                shutil.rmtree(out_dir, ignore_errors=True)
            self.save_state()
        finally:
            if not self.args.keep_work:
                shutil.rmtree(work, ignore_errors=True)

    def _backend_order(self) -> List[str]:
        if self.args.backend == "fontmake":
            return ["fontmake"]
        if self.args.backend == "fontc":
            return ["fontc"]
        return ["fontc", "fontmake"] if self.args.fontc_bin else ["fontmake"]

    # ---- status snapshot file (for the monitor UI / detached builds) + timings report
    def _write_status(self):
        with self._status_lock:                  # serialize writers (writer thread + final write)
            try:
                tmp = self.build_dir / "status.json.tmp"
                tmp.write_text(json.dumps(self.snapshot()))
                tmp.replace(self.build_dir / "status.json")
            except OSError:
                pass

    def _status_writer(self):
        while not self._status_stop.is_set():
            self._write_status()
            self._status_stop.wait(1.0)

    def write_timings(self):
        snap = self.snapshot()
        with self.lock:
            fams = {s: {k: round(v, 2) for k, v in r.timings.items()}
                    for s, r in self.results.items() if r.timings}
        data = {"elapsed": round(snap["elapsed"], 1), "phases": snap["phase_durations"],
                "operations": snap["op_stats"], "families": fams}
        try:
            (self.build_dir / "timings.json").write_text(json.dumps(data, indent=1))
        except OSError:
            pass
        return data

    def migration_report(self):
        """fontc→fontmake migration tracking: who builds with fontc, who still needs fontmake
        (and why fontc failed = the blockers), and 'both'-mode agreement."""
        with self.lock:
            rs = list(self.results.values())
        built = [r for r in rs if r.status == "built"]
        fontc = [r.slug for r in built if r.backend == "fontc"]
        fallback = [{"slug": r.slug, "fontc_error": r.fontc_error}
                    for r in built if r.backend == "fontmake" and r.fontc_error]
        fm_only = [r.slug for r in built if r.backend == "fontmake" and not r.fontc_error]
        both = [r for r in built if r.backend == "both"]
        identical = [r.slug for r in both if r.vs == "identical"]
        differ = [{"slug": r.slug, "vs": r.vs} for r in both if r.vs and r.vs != "identical"]
        failed = [{"slug": r.slug, "error": r.error, "fontc_error": r.fontc_error}
                  for r in rs if r.status == "failed"]
        data = {
            "summary": {"fontc": len(fontc), "fontmake_fallback": len(fallback),
                        "fontmake_only": len(fm_only), "both_identical": len(identical),
                        "both_differ": len(differ), "failed": len(failed)},
            "fontc_built": fontc,
            "fontmake_fallback": fallback,        # fontc failed → fontmake used (MIGRATION BLOCKERS)
            "fontmake_only": fm_only,             # fontmake without trying fontc
            "both": {"fontc_ok": sum(1 for r in both if r.fontc_ok),
                     "fontmake_ok": sum(1 for r in both if r.fontmake_ok),
                     "identical": identical, "differ": differ},
            "failed": failed,
        }
        try:
            (self.build_dir / "migration.json").write_text(json.dumps(data, indent=1))
        except OSError:
            pass
        return data

    # ---- lifecycle: a background driver runs the phases (archive → cohorts → build)
    def run(self):
        self._status_thread = threading.Thread(target=self._status_writer, daemon=True)
        self._status_thread.start()
        self._size_thread = threading.Thread(target=self._size_writer, daemon=True)
        self._size_thread.start()
        self._control_thread = threading.Thread(target=self._control_watcher, daemon=True)
        self._control_thread.start()
        self.driver = threading.Thread(target=self._drive, daemon=True)
        self.driver.start()

    # ---- live config: a monitor writes control.json; the daemon applies it on the fly ----
    def _control_watcher(self):
        path = self.build_dir / "control.json"
        try:                                          # ignore a stale control from a prior run
            self._control_last = int(json.loads(path.read_text()).get("seq", 0))
        except Exception:
            self._control_last = 0
        while not self._status_stop.is_set():
            try:
                ctl = json.loads(path.read_text())
                seq = int(ctl.get("seq", 0))
                if seq > self._control_last:
                    self._control_last = seq
                    self.apply_live(ctl.get("set", {}) or {})
            except Exception:
                pass
            self._status_stop.wait(0.5)

    def apply_live(self, settings: dict):
        """Apply a live config change to the RUNNING build (no restart): bump percent → enqueue
        the newly-included families (fetch + cohort + build them); bump jobs → spawn more
        workers; backend/timeout/compare/populate_archive update args for subsequent builds."""
        if self.stop.is_set():                        # build finished/stopping → can't apply live
            with self.lock:
                self.control_log.append(
                    f"[{hms(self.snapshot_elapsed())}] ignored (build finished) — restart (C) to change")
                del self.control_log[:-50]
            return
        changed: List[str] = []
        with self.lock:
            for k in ("backend", "timeout", "compare", "populate_archive"):
                if k in settings and getattr(self.args, k, None) != settings[k]:
                    setattr(self.args, k, settings[k])
                    changed.append(f"{k}={settings[k]}")
        new_pct = settings.get("percent")
        if isinstance(new_pct, (int, float)) and float(new_pct) != self.args.percent:
            added = self._extend_worklist(float(new_pct))
            with self.lock:
                self.args.percent = float(new_pct)
            changed.append(f"percent={float(new_pct):g} (+{added} families)")
        new_jobs = settings.get("jobs")
        if isinstance(new_jobs, int) and new_jobs >= 1:
            self._ensure_workers(new_jobs)
            with self.lock:
                if new_jobs != self.args.jobs:
                    changed.append(f"jobs={new_jobs}")
                self.args.jobs = new_jobs
        if changed:
            with self.lock:
                self.control_log.append(f"[{hms(self.snapshot_elapsed())}] " + ", ".join(changed))
                del self.control_log[:-50]
            self._emit("control", "", changes=changed)

    def snapshot_elapsed(self) -> float:
        return self._resumed_elapsed + (time.time() - self.start_time)

    def _ensure_workers(self, target_jobs: int):
        """Bring the live worker pool up to `target_jobs` (only grows — decreasing just lets the
        extra workers drain). Count + spawn happen under ONE lock so two callers (driver respawn
        + a jobs-bump) can't each see alive=0 and double-spawn. Safe only while not stopping."""
        if self.stop.is_set():
            return
        with self.lock:
            alive = sum(1 for t in self.workers if t.is_alive())
            for _ in range(alive, max(1, target_jobs)):
                self._wid_counter += 1
                t = threading.Thread(target=self.worker, args=(self._wid_counter,), daemon=True)
                self.workers.append(t)
                t.start()                             # worker's first action is q.get (no lock)

    def _extend_worklist(self, new_pct: float) -> int:
        """Enqueue families newly included by a higher percent (or all of --only). Queuing makes
        all_done() False, so the running build loop keeps going and the workers pick them up."""
        if not self._all_families or self.stop.is_set():
            return 0
        if self.args.only:
            keep = set(self.args.only.split(","))
            sample = [f for f in self._all_families if f.slug in keep]
        else:
            sample = sample_evenly(self._all_families, new_pct)
        fresh = []
        with self.lock:
            for f in sample:
                if f.slug not in self.results:        # not already queued/built/failed
                    self.families[f.slug] = f
                    self.results[f.slug] = Result(slug=f.slug, status="queued")
                    fresh.append(f)
            bt = self._task("build")
            if bt is not None:
                bt.total = len(self.results)
                if bt.status in ("done", "failed"):   # build had wrapped up — reopen it
                    bt.status, bt.t1 = "running", 0.0
        for f in fresh:
            self.q.put(f.slug)
        if fresh:
            self._ensure_workers(self.args.jobs)      # respawn any workers that had exited
            if self.args.populate_archive:            # pre-fetch the new repos in the background
                urls = sorted({f.repo_url for f in fresh})
                threading.Thread(target=lambda: populate_archive(
                    urls, self.archive, self.args.jobs,
                    on_progress=lambda d, n, u, st, m='': self._archive_progress(d, u, st, m),
                    stop=self.stop, clone_lock=self.clone_locks), daemon=True).start()
        return len(fresh)

    def _drive(self):
        try:
            # Task: clone google/fonts (shallow) if the worklist needs METADATA and it's absent.
            t = self._task("clone_gf")
            if t is not None and t.status == "pending" and not self.stop.is_set():
                self._task_start("clone_gf")
                try:
                    ensure_google_fonts(
                        self.google_fonts,
                        on_progress=lambda m: self._task_progress("clone_gf", 0, m))
                    self._task_done("clone_gf", str(self.google_fonts))
                except (RuntimeError, ValueError) as e:
                    self._task_fail("clone_gf", str(e))
                    return

            # Task: build fontc from source (cargo build --release) if requested.
            t = self._task("build_fontc")
            if t is not None and t.status == "pending" and not self.stop.is_set():
                self._task_start("build_fontc")
                try:
                    dest = Path(getattr(self.args, "_data_dir", str(self.build_dir))) / "fontc"
                    self.args.fontc_bin = build_fontc_from_source(
                        dest, on_progress=lambda m: self._task_progress("build_fontc", 0, m))
                    self._task_done("build_fontc", self.args.fontc_bin)
                except RuntimeError as e:
                    self._task_fail("build_fontc", str(e))
                    return

            # Task: discover the worklist (METADATA-driven or archive-driven) and enqueue it.
            if not self.stop.is_set():
                self._task_start("discover")
                if self.args.source == "archive":
                    fams, total, skipped = discover_from_archive(
                        self.archive, self.args.archive_rev, self.args.jobs)
                else:
                    fams, total, skipped = discover(self.google_fonts)
                self._all_families = fams            # keep the full list for live percent bumps
                if self.args.only:                   # --only restricts the WHOLE pipeline
                    keep = set(self.args.only.split(","))
                    fams = [f for f in fams if f.slug in keep]   # (so a targeted rebuild only
                else:                                            #  mirrors/scans/builds those)
                    fams = sample_evenly(fams, self.args.percent)
                with self.lock:
                    self.families = {f.slug: f for f in fams}
                    self.total_with_source = total
                    self.skipped_no_config = skipped
                self._enqueue()
                self._task_done(
                    "discover",
                    f"{self.q.qsize()} queued of {len(fams)} selected "
                    + (f"(incl. {self._enqueued_retries} retries) " if self._enqueued_retries else "")
                    + f"({self.args.percent:g}%; {total} with source, {skipped} skipped)")

            # ---- DYNAMIC PIPELINE: archive pre-warm + builds run CONCURRENTLY (no barriers) ----
            # The workers are self-sufficient: each one mirrors-on-demand, assigns its cohort, and
            # compiles its family the moment that family's repo is available. A background archive
            # pre-warmer mirrors missing repos ahead of the builders (idle I/O overlapping CPU
            # builds), sharing per-repo clone locks so no repo is ever cloned twice. So nothing
            # blocks on a global "mirror everything, then scan cohorts, then build" barrier.
            if self.q.qsize() and not self.stop.is_set():
                if self.venvs is not None:
                    self.venvs.ensure_base()

                # (a) concurrent archive pre-warmer (only if --populate-archive)
                prewarm = None
                at = self._task("archive")
                if at is not None and at.status == "pending" and self.families:
                    urls = sorted({f.repo_url for f in self.families.values()})
                    self._task_running("archive", len(urls))

                    def _prewarm():
                        added, failed, present = populate_archive(
                            urls, self.archive, self.args.jobs,
                            on_progress=lambda d, n, u, st, m='': self._archive_progress(d, u, st, m),
                            stop=self.stop, clone_lock=self.clone_locks)
                        self._task_done(  # "unreachable" ≠ build failures
                            "archive",
                            f"{len(added)} mirrored, {present} present, {len(failed)} unreachable")
                    prewarm = threading.Thread(target=_prewarm, daemon=True)
                    prewarm.start()

                # (b) build workers — start immediately; they clone-on-demand + cohort-on-demand
                with self.lock:
                    build_total = len(self.results)   # queued now + any already-built (resume)
                self._task_start("build", build_total)
                self._ensure_workers(self.args.jobs)  # spawn the initial pool (atomic)
                # Completion is decided here, NOT by setting the global `stop` on all_done():
                # that lets a live percent-bump re-open the build. The workers self-exit when
                # (all_done and queue empty); when none are alive we re-check, UNDER THE LOCK,
                # whether a live bump queued more work — if so, respawn and keep going; else the
                # build is truly done. `stop` stays reserved for shutdown (Ctrl-C / --stop).
                while True:
                    if self.stop.is_set():
                        break
                    with self.lock:
                        alive = any(th.is_alive() for th in self.workers)
                        pending = (not self.q.empty()) or any(
                            r.status == "queued" for r in self.results.values())
                        done_n = sum(1 for r in self.results.values()
                                     if r.status in ("built", "failed", "skipped"))
                    if not alive:
                        if pending:
                            self._ensure_workers(self.args.jobs)   # live bump → resume building
                            continue
                        break                          # build truly complete
                    self._task_progress("build", done_n)
                    time.sleep(0.2)
                self.stop.set()                        # build done → abort the pre-warmer's clones
                # the build task's OUTCOME, not just "processed 15/15": show built/failed, and
                # mark it failed (❌, not ✅) when every build failed — so it never looks like a
                # success when nothing built.
                with self.lock:
                    nb = sum(1 for r in self.results.values() if r.status == "built")
                    nf = sum(1 for r in self.results.values() if r.status == "failed")
                    bt = self._task("build")
                    if bt is not None:
                        bt.status = "failed" if (nb == 0 and nf > 0) else "done"
                        bt.t1 = time.time()
                        bt.detail = f"{nb} built, {nf} failed"

                # (c) let the pre-warmer wind down. The build loop already set `stop` once builds
                # finished, which aborts any remaining clones; this just joins it cleanly.
                if prewarm is not None:
                    prewarm.join(timeout=10)
                    at = self._task("archive")
                    if at is not None and at.status == "running":
                        self._task_done("archive", "stopped (build finished)")
            elif self._task("build") is not None and self._task("build").status == "pending":
                if not self.families:
                    done_note = "nothing to build (no families discovered)"
                else:
                    with self.lock:
                        built = sum(1 for s in self.families if s in self.results
                                    and self.results[s].status == "built")
                        failed = sum(1 for s in self.families if s in self.results
                                     and self.results[s].status == "failed")
                    if failed:                       # all failures here were non-retryable causes
                        done_note = (f"nothing queued: {built} already built, {failed} failed with "
                                     f"non-retryable causes — turn on 'retry failed' to force them, "
                                     f"or raise % to add more families")
                    else:
                        done_note = f"nothing to build: all {built} selected families already built"
                self._task_done("build", done_note)
        except Exception as e:
            with self.lock:
                self.phase_error = str(e)
                cur = self._task(self.phase)
                if cur is not None and cur.status == "running":
                    cur.status, cur.t1, cur.detail = "failed", time.time(), str(e)
        finally:
            self.save_state()
            self._set_phase("done")  # workers are guaranteed stopped here
            self._status_stop.set()                       # stop the periodic writer + control…
            if self._control_thread is not None:          # join the control watcher so no
                self._control_thread.join(timeout=3)       # apply_live() runs after final status
            if self._status_thread is not None:
                self._status_thread.join(timeout=3)
            self.write_timings()
            self.migration_report()
            self._write_status()                          # …then write the final status alone

    def _close_events(self):
        with self._events_lock:
            if not self._events_closed:
                self._events_closed = True
                try:
                    self._events.close()
                except Exception:
                    pass

    def join(self):
        if self.driver is not None:
            self.driver.join()
        self._close_events()   # after the driver (and thus all workers) have stopped


# ============================================================================ frontends

def human(n: float) -> str:
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if abs(n) < 1024:
            return f"{n:.0f}{unit}" if unit == "B" else f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}PiB"


def hms(secs: float) -> str:
    secs = int(secs)
    return f"{secs // 3600:02d}:{(secs % 3600) // 60:02d}:{secs % 60:02d}"


def _read_log_tail(path: Path, n: int = 120) -> List[str]:
    """Last `n` lines of a per-family log, for the failure detail overlay."""
    try:
        lines = Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return ["(log not available)"]
    return lines[-n:] if lines else ["(empty log)"]


class Frontend:
    """Base frontend: observe an Orchestrator and render progress. Subclass and register
    in FRONTENDS, or write your own (e.g. web) that tails <build-dir>/events.jsonl +
    state.json out-of-process."""
    def __init__(self, orch):
        self.orch = orch
        self.monitor = False   # True when attached read-only to a (possibly detached) build
    def run(self):
        raise NotImplementedError


class NoneFrontend(Frontend):
    def run(self):
        # works for both a real build (snapshot done == phase done) and a monitor (status.json)
        while not self.orch.snapshot().get("done", False):
            if self.orch.stop.is_set():
                break
            time.sleep(0.3)
        if not self.monitor:
            self.orch.stop.set()


class PlainFrontend(Frontend):
    """Traditional terminal output: one line per completion + periodic summaries."""
    def run(self):
        seen, last, last_phase = set(), 0.0, None
        while True:
            snap = self.orch.snapshot()
            ph = snap["phase"]
            if ph != last_phase:
                print(f"== phase: {ph} ==", flush=True)
                last_phase = ph
            with self.orch.lock:
                done = [(r.slug, r.status, r.backend, r.error, r.dur(), r.out_missing)
                        for r in self.orch.results.values()
                        if r.status in ("built", "failed") and r.slug not in seen]
            for slug, status, backend, error, dur, missing in done:
                seen.add(slug)
                if status == "built":
                    extra = f"  (partial: missing {missing})" if missing else ""
                    print(f"[OK ] {slug}  ({backend}, {dur:.0f}s){extra}", flush=True)
                else:
                    print(f"[FAIL] {slug}  {error}", flush=True)
            now = time.time()
            if now - last > 5:
                c = snap["counts"]
                if ph in ("archive", "cohorts") and snap["phase_total"]:
                    print(f"  -- {hms(snap['elapsed'])}  {ph}: {snap['phase_done']}/"
                          f"{snap['phase_total']}  build {human(snap.get('disk_build_total',0))}", flush=True)
                else:
                    print(f"  -- {hms(snap['elapsed'])}  built {c['built']} failed {c['failed']} "
                          f"building {c['building']} queued {c['queued']}  "
                          f"build {human(snap.get('disk_build_total',0))} "
                          f"[fontc {snap['backends']['fontc']}/fontmake {snap['backends']['fontmake']}]",
                          flush=True)
                last = now
            if snap["done"]:
                self.orch.stop.set()
                break
            time.sleep(0.5)


class JsonFrontend(Frontend):
    """Emit newline-delimited JSON snapshots to stdout (machine/web consumable).
    Per-event detail is also in <build-dir>/events.jsonl."""
    def run(self):
        while True:
            snap = self.orch.snapshot()
            print(json.dumps(snap), flush=True)
            if snap["done"]:
                self.orch.stop.set()
                break
            time.sleep(1.0)


class CursesFrontend(Frontend):
    """Optional ncurses dashboard (A built, B building, C disk, D elapsed, E failures)."""
    def run(self):
        try:
            import curses
        except Exception as e:
            if getattr(self, "setup", False):
                raise                                 # main falls back to the plain setup confirm
            print(f"curses unavailable ({e}); using --ui plain.", file=sys.stderr)
            return PlainFrontend(self.orch).run()
        try:
            locale.setlocale(locale.LC_ALL, "")   # enable UTF-8 wide chars (emoji status marks)
        except locale.Error:
            pass
        try:
            return curses.wrapper(self._draw)        # config dict (▶ Start) / "reconfigure" (C) / None
        except Exception as e:
            if getattr(self, "setup", False):
                raise                                 # the plain setup path can't run via Plain UI
            print(f"curses error ({e}); switching to plain output.", file=sys.stderr)
            return PlainFrontend(self.orch).run()

    PHASE_LABEL = {"init": "starting…", "clone_gf": "cloning google/fonts",
                   "build_fontc": "building fontc from source",
                   "discover": "discovering worklist",
                   "archive": "populating archive (mirroring repos)",
                   "cohorts": "scanning dependency cohorts", "build": "building", "done": "done"}
    # emoji status marks (ASCII fallback chosen at runtime if the terminal can't render them)
    EMOJI = {"pending": "⏳", "running": "🔄", "done": "✅", "failed": "❌", "skipped": "➖"}
    ASCII = {"pending": "..", "running": ">>", "done": "OK", "failed": "XX", "skipped": "--"}
    VIEWS = ("config", "overview", "cohorts", "built", "failures", "stats")

    # Full config schema for the ONE Configuration tab (first-run setup AND live editing).
    # `live`=True fields can change on a running build; the rest need a restart.
    CONFIG_SCHEMA = [
        {"key": "source", "label": "worklist source", "type": "choice",
         "choices": ["metadata", "archive"], "live": False},
        {"key": "google_fonts", "label": "google/fonts clone", "type": "path", "live": False,
         "show_if": lambda v: v.get("source") == "metadata"},
        {"key": "archive", "label": "repo archive", "type": "path", "live": False},
        {"key": "build_dir", "label": "build output dir", "type": "path", "live": False},
        {"key": "backend", "label": "build backend", "type": "choice",
         "choices": ["auto", "fontc", "fontmake", "both"], "live": True},
        {"key": "fontc_bin", "label": "fontc binary", "type": "path", "live": False,
         "show_if": lambda v: v.get("backend") != "fontmake"},
        {"key": "build_fontc", "label": "build fontc from source (if none)", "type": "bool",
         "live": False, "show_if": lambda v: v.get("backend") != "fontmake" and not v.get("fontc_bin")},
        {"key": "jobs", "label": "parallel jobs", "type": "stepnum", "step": 1, "min": 1,
         "max": 256, "live": True},
        {"key": "percent", "label": "percent of library", "type": "stepnum", "step": 5,
         "min": 1, "max": 100, "live": True},
        {"key": "timeout", "label": "per-build timeout (0=off)", "type": "stepnum", "step": 30,
         "min": 0, "max": 100000, "live": True},
        {"key": "populate_archive", "label": "populate archive (fetch repos)", "type": "bool",
         "live": True},
        {"key": "manage_venvs", "label": "cohort venvs", "type": "bool", "live": False},
        {"key": "retry_failed", "label": "retry ALL failed (incl. genuine errors)", "type": "bool",
         "live": False},
        {"key": "compare", "label": "compare to shipped", "type": "bool", "live": True,
         "show_if": lambda v: v.get("source") == "metadata"},
    ]

    @staticmethod
    def _effective_config(snap: dict) -> dict:
        """The settings to show in the config tab: the live build's config if reported, else the
        persisted config file (so it reflects real current settings, not a list of None)."""
        cfg = snap.get("config") or {}
        if not cfg and snap.get("config_path"):
            cfg = load_config(Path(snap["config_path"]))
        return cfg

    @classmethod
    def _cfg_init_fields(cls, cfg: dict) -> list:
        """Build editable field descriptors (value string + caret) from a config dict."""
        fields = []
        for sc in cls.CONFIG_SCHEMA:
            f = dict(sc)
            v = cfg.get(f["key"])
            if f["type"] == "bool":
                f["value"] = bool(v)
            elif f["type"] == "choice":
                f["value"] = v if v in f["choices"] else f["choices"][0]
            else:                                     # path / stepnum (edited as text)
                if f["key"] == "timeout":
                    v = 0 if not v else int(v)
                f["value"] = "" if v is None else (f"{v:g}" if isinstance(v, float) else str(v))
                f["_caret"] = len(f["value"])
            fields.append(f)
        return fields

    @staticmethod
    def _cfg_typed(fields: list) -> dict:
        out = {}
        for f in fields:
            t, v = f["type"], f["value"]
            if t == "bool":
                out[f["key"]] = bool(v)
            elif t == "choice":
                out[f["key"]] = v
            elif t == "stepnum":
                try:
                    x = float(v)
                except (TypeError, ValueError):
                    x = 0.0
                out[f["key"]] = int(x) if x == int(x) else x
            else:
                out[f["key"]] = v
        if out.get("timeout") in (0, 0.0):
            out["timeout"] = None                     # 0 → no timeout
        return out

    @staticmethod
    def _cfg_visible(fields: list, vals: dict) -> list:
        return [f for f in fields if "show_if" not in f or f["show_if"](vals)]

    @staticmethod
    def _cfg_field_key(f: dict, ch: int):
        """Edit field `f` from a keypress (wizard-style: ←/→ caret/step/cycle, type, space).
        Returns 'advance' on Enter (move to next field), else None."""
        import curses
        t = f["type"]
        if t == "bool":
            if ch in (ord(" "), 10, 13):
                f["value"] = not f["value"]
            return None
        if t == "choice":
            ci = f["choices"].index(f["value"]) if f["value"] in f["choices"] else 0
            if ch in (ord(" "), curses.KEY_RIGHT):
                f["value"] = f["choices"][(ci + 1) % len(f["choices"])]
            elif ch == curses.KEY_LEFT:
                f["value"] = f["choices"][(ci - 1) % len(f["choices"])]
            elif ch in (10, 13):
                return "advance"
            return None
        cur = f.get("_caret", len(f["value"]))        # path / stepnum text
        if t == "stepnum" and ch in (curses.KEY_LEFT, curses.KEY_RIGHT):
            step = f.get("step", 5) * (1 if ch == curses.KEY_RIGHT else -1)
            try:
                x = float(f["value"] or 0)
            except ValueError:
                x = 0.0
            x = max(f.get("min", 0), min(f.get("max", 10 ** 9), x + step))
            f["value"] = f"{x:g}"
            f["_caret"] = len(f["value"])
        elif ch == curses.KEY_LEFT:
            f["_caret"] = max(0, cur - 1)
        elif ch == curses.KEY_RIGHT:
            f["_caret"] = min(len(f["value"]), cur + 1)
        elif ch == curses.KEY_HOME:
            f["_caret"] = 0
        elif ch == curses.KEY_END:
            f["_caret"] = len(f["value"])
        elif ch in (curses.KEY_BACKSPACE, 127, 8):
            if cur > 0:
                f["value"] = f["value"][:cur - 1] + f["value"][cur:]
                f["_caret"] = cur - 1
        elif ch == curses.KEY_DC:
            if cur < len(f["value"]):
                f["value"] = f["value"][:cur] + f["value"][cur + 1:]
        elif ch in (10, 13):
            return "advance"
        elif 32 <= ch < 127:
            c = chr(ch)
            if t in ("text", "path") or (t == "stepnum" and (c.isdigit() or c == ".")):
                f["value"] = f["value"][:cur] + c + f["value"][cur:]
                f["_caret"] = cur + 1
        return None

    FIELD_HELP = {
        "source": "where the worklist comes from: google/fonts METADATA, or every mirror in the archive",
        "google_fonts": "path to a google/fonts clone (cloned shallow if absent)",
        "archive": "the bare-mirror repo archive (append-only; never deleted)",
        "build_dir": "where all build assets go (out/ venvs/ logs/) — never inside a repo",
        "backend": "auto = fontc first then fall back to fontmake · fontc/fontmake = that one · both = build & compare",
        "fontc_bin": "path to the fontc (Rust) binary",
        "build_fontc": "no fontc binary? build it from source with cargo",
        "jobs": "how many families build in parallel",
        "percent": "build only this % of the library (evenly-spaced sample); raise it live to build more",
        "timeout": "per-build timeout in seconds (0 = never time out)",
        "populate_archive": "mirror any missing upstream repos into the archive while building",
        "manage_venvs": "create & share one venv per dependency cohort",
        "retry_failed": "also re-attempt families that failed with genuine build errors (fixable "
                        "causes — broken venvs, transient fetches — are always retried)",
        "compare": "sha256-compare each built font to the shipped one (metadata source only)",
    }

    def _focus_info(self, kind, item, title=""):
        """A short, context-sensitive description of the currently focused item, for the always-on
        status panel. Returns 1-3 lines (grows only when useful)."""
        if kind == "field":
            return [f"{item['label']} — {self.FIELD_HELP.get(item['key'], 'edit with ←/→ or type')}"]
        if kind == "action":
            return [{"▶ Start build": "▶ launch the build with these settings",
                     "Cancel": "discard and exit — nothing is built",
                     "✓ apply changes": "apply the edited live settings to the running build now",
                     }.get(item, str(item))]
        if kind == "overview":                            # a pipeline task
            return [f"{item['name']}: {item['status']}"
                    + (f" — {item['detail']}" if item.get("detail") else "")]
        if kind == "building":
            return [f"building {item['slug']} — step '{item.get('note') or item.get('backend') or 'starting'}'"
                    f"  (worker {item['worker']}, {hms(item['dur'])})"]
        if kind == "failures":
            return [f"{item['slug']}  FAILED:", "  " + str(item.get("error", ""))]
        if kind == "built":
            return [f"{item['slug']} ✓ built with {item.get('backend', '?')} — "
                    f"{human(item.get('bytes', 0))}, vs shipped: {item.get('compare') or 'not compared'}"]
        if kind == "cohorts":
            reqs = (item.get("requirements") or "").splitlines()
            fams = item.get("families", [])
            return [f"cohort {item['key']}: {item['count']} families"
                    + (f" — needs {reqs[0]}" if reqs else " (base — no extra requirements)"),
                    "  " + (", ".join(fams) if fams else "(none assigned yet)")]
        if kind == "failcat":                             # a failure-cause bucket {cat,count,hint}
            return [f"{item['count']} families failed: {item['cat']}", "  → " + item.get("hint", "")]
        if kind == "stats":                               # an op: (op, {total,count,mean,max})
            op, st = item
            return [f"{op}: total {st['total']}s · n {st['count']} · mean {st['mean']}s · max {st['max']}s"]
        if kind is None and isinstance(item, dict) and "repo" in item:   # archive entry
            repo, reason = item.get("repo"), item.get("reason", "")
            if item.get("status") == "failed":
                return [f"✗ {repo} — could NOT be mirrored into the archive",
                        "  " + (reason or "git clone failed: repo unreachable, removed, renamed, or private")]
            return [f"+ {repo} — newly mirrored into the archive (a fresh bare clone)"]
        if kind is None and isinstance(item, tuple) and len(item) == 2:  # phase timing
            return [f"phase {item[0]}: took {hms(item[1])}"]
        return []

    def _cfg_apply_live(self, cfg_fields: list, snap: dict) -> None:
        """Live 'apply': write the changed live-editable fields to control.json for the daemon."""
        new = self._cfg_typed(cfg_fields)
        live_cfg = self._effective_config(snap)
        live_keys = {f["key"] for f in self.CONFIG_SCHEMA if f.get("live")}
        changed = {k: v for k, v in new.items() if k in live_keys and v != live_cfg.get(k)}
        if changed:
            write_control(self.orch.build_dir, changed)

    def _detail_lines(self, view: str, item) -> List[str]:
        """Full detail for the selected list item, shown in the overlay."""
        out: List[str] = []
        if view == "overview":                       # a pipeline Task dict
            out += [f"Task: {item.get('name', '')}", f"key: {item.get('key', '')}",
                    f"status: {item.get('status', '')}",
                    f"elapsed: {hms(item.get('elapsed', 0))}"]
            if item.get("total"):
                out.append(f"progress: {item['done']}/{item['total']}")
            if item.get("detail"):
                out += ["", "detail:", "  " + str(item["detail"])]
        elif view == "failcat":                      # {cat, count, hint}
            out += [f"Failure cause: {item.get('cat', '')}",
                    f"families affected: {item.get('count', 0)}", "",
                    "what to do:", "  " + item.get("hint", "")]
        elif view == "cohorts":                      # {key, count, requirements, families}
            fams = item.get("families", [])
            out += [f"Cohort: {item.get('key', '')}", f"families: {item.get('count', 0)}", "",
                    "family names:"]
            out += ["  " + n for n in fams] or ["  (none assigned yet)"]
            out += ["", "requirements:"]
            reqs = (item.get("requirements") or "").splitlines()
            out += ["  " + r for r in reqs] or ["  (none — the 'base' cohort has no requirements file)"]
        elif view == "building":                     # {slug, worker, dur, backend, note}
            slug = item.get("slug", "")
            logp = self.orch.build_dir / "logs" / (slug.replace("/", "__") + ".log")
            out += [f"Building: {slug}", f"worker: {item.get('worker', '')}",
                    f"elapsed: {hms(item.get('dur', 0))}",
                    f"step: {item.get('note') or item.get('backend') or '(starting)'}",
                    f"log: {logp}", "", "log tail:"]
            out += ["  " + ln for ln in _read_log_tail(logp, 60)]
        elif view == "built":                        # {slug, backend, bytes, compare, log}
            slug = item.get("slug", "")
            out += [f"Built: {slug}",
                    f"backend: {item.get('backend', '')}",
                    f"output size: {human(item.get('bytes', 0))}",
                    f"vs shipped: {item.get('compare') or '(not compared)'}",
                    f"fonts: {self.orch.build_dir / 'out' / slug.replace('/', '__')}",
                    f"rebuild: python3 gflib_build.py --only {slug} --rebuild --yes"]
            log = item.get("log", "")
            if log:
                out += ["", "log tail:"]
                out += ["  " + ln for ln in _read_log_tail(self.orch.build_dir / log, 60)]
        elif view == "failures":                     # {slug, error, log}
            log = item.get("log", "")
            slug = item.get("slug", "")
            out += [f"Failed: {slug}",
                    f"rebuild: python3 gflib_build.py --only {slug} --rebuild --yes",
                    "", "error:", "  " + str(item.get("error", "")),
                    "", f"log: {self.orch.build_dir / log if log else '(none)'}", "", "log tail:"]
            if log:
                out += ["  " + ln for ln in _read_log_tail(self.orch.build_dir / log, 120)]
        elif view == "stats":                        # (op, {total,count,mean,max})
            op, s = item
            out += [f"Operation: {op}", f"total: {s['total']} s", f"count: {s['count']}",
                    f"mean: {s['mean']} s", f"max: {s['max']} s"]
        return out

    @staticmethod
    def _layout_sections(avail, sizes, fi, sel):
        """Plan how to stack multiple sections into `avail` body rows. Returns a list of entries
        {idx, top, count, hint, none} for the sections to draw (header always = 1 row each).

        Guarantees (verified by the section_layout sweep test):
          * total rows used (headers + items + hints + the '(none)' line) never exceeds `avail`
            — the focused section can NEVER push later sections off-screen (the old bug);
          * the focused section is always included, and its selected item `sel` is always within
            the rendered window, whenever it has items and avail >= 2.
        Trailing blank separators are NOT planned here; the caller draws them only from leftover
        slack, so they can never cause an overflow."""
        n = len(sizes)
        if n == 0 or avail < 2:                      # need >= header + 1 content row to show anything
            return []
        fi = fi if 0 <= fi < n else 0
        cap_sections = max(1, avail // 2)            # each shown section needs >= header + 1 row
        if n <= cap_sections:
            show = list(range(n))
        else:                                        # too tight for all: keep focused + neighbours
            show = [fi]
            for i in list(range(fi + 1, n)) + list(range(fi - 1, -1, -1)):
                if len(show) >= cap_sections:
                    break
                show.append(i)
            show = sorted(show)
        m = len(show)
        alloc = {i: 1 for i in show}                 # content rows per section (>=1: an item / "(none)")
        extra = max(0, avail - 2 * m)                # rows beyond header + 1 content each
        if extra > 0 and sizes[fi] > 1:              # grow the focused section first (selection visible)
            g = min(sizes[fi] - 1, extra); alloc[fi] += g; extra -= g
        active = [i for i in show if i != fi and sizes[i] > alloc[i]]
        while active and extra > 0:                  # water-fill the rest fairly
            share = extra // len(active)
            if share == 0:
                for i in sorted(active, key=lambda j: -sizes[j]):
                    if extra <= 0:
                        break
                    if alloc[i] < sizes[i]:
                        alloc[i] += 1; extra -= 1
                break
            for i in list(active):
                g = min(share, sizes[i] - alloc[i]); alloc[i] += g; extra -= g
                if alloc[i] >= sizes[i]:
                    active.remove(i)
        if extra > 0 and alloc[fi] < sizes[fi]:      # focused soaks any remainder
            g = min(sizes[fi] - alloc[fi], extra); alloc[fi] += g
        out = []
        for i in show:
            size, a = sizes[i], alloc[i]
            if size == 0:
                out.append({"idx": i, "top": 0, "count": 0, "hint": False, "none": True})
                continue
            if a >= size:
                count, hint = size, False
            elif a == 1:
                count, hint = 1, False               # one row: show an item, no room for a hint
            else:
                count, hint = a - 1, True            # items + a "(+N more)" row
            top = 0
            if i == fi and sel >= count:             # scroll so the selected item stays visible
                top = min(sel - count + 1, size - count)
            out.append({"idx": i, "top": top, "count": count, "hint": hint, "none": False})
        return out

    def _draw(self, stdscr):
        import curses
        stdscr.nodelay(True)
        stdscr.keypad(True)   # map arrow keys to curses.KEY_UP/KEY_DOWN (defensive; wrapper also does this)
        try:
            curses.curs_set(0)
        except curses.error:
            pass
        has_color = False
        try:
            curses.start_color(); curses.use_default_colors()
            for i, col in enumerate((curses.COLOR_GREEN, curses.COLOR_RED,
                                     curses.COLOR_YELLOW, curses.COLOR_CYAN), 1):
                curses.init_pair(i, col, -1)
            has_color = True
        except curses.error:
            pass
        if has_color:
            GREEN, RED, YEL, CYAN = (curses.color_pair(i) for i in range(1, 5))
        else:
            GREEN = RED = YEL = CYAN = curses.A_NORMAL
        # decide once whether the terminal can render emoji status marks
        use_emoji = True
        try:
            stdscr.addstr(0, 0, self.EMOJI["done"]); stdscr.erase()
        except curses.error:
            use_emoji = False
        MARK = self.EMOJI if use_emoji else self.ASCII
        SATTR = {"done": GREEN, "failed": RED, "running": YEL, "skipped": curses.A_DIM,
                 "pending": curses.A_NORMAL}

        # Each non-config tab is a list of SECTIONS: (title, items, fmt, color, detail-view).
        # ←/→ move focus between sections, ↑/↓ navigate items in the focused one, ↵ acts on the
        # selected item. (title, items, fmt(item)->str, color(item)->attr, detail-view or None)
        def sections_for(snap, v):
            if v == "overview":
                secs = [("Pipeline", snap.get("tasks", []),
                         lambda t: f"{MARK.get(t['status'], '?')} {t['name']:<26} "
                         f"{((str(t['done'])+'/'+str(t['total'])) if t['total'] else ''):<11}"
                         f"{(hms(t['elapsed']) if t['elapsed'] else ''):>8}  {t['detail']}",
                         lambda t: SATTR.get(t["status"], 0), "overview")]
                arch = snap.get("archive_recent", [])
                if arch:
                    secs.append(("Archive — mirrored", arch,
                                 lambda a: ("+ " if a["status"] == "added" else "✗ ") + a["repo"],
                                 lambda a: RED if a["status"] == "failed" else GREEN, None))
                secs.append(("Now building", snap.get("building", []),
                             lambda b: f"w{b['worker']:>2} {b['slug']:<34} {hms(b['dur']):>8}  "
                             f"{b['note'] or b['backend'] or ''}", lambda b: YEL, "building"))
                secs.append(("Recent failures", snap.get("failures_recent", []),
                             lambda f: f"{f['slug']:<34} {f['error']}", lambda f: RED, "failures"))
                return secs
            if v == "cohorts":
                return [("Dependency cohorts", snap.get("cohorts", []),
                         lambda co: "%4d  %-14s %s" % (co["count"], co["key"],
                         (", ".join(co.get("families", [])) or "(no families assigned yet)")),
                         lambda co: 0 if co["key"] == "base" else CYAN, "cohorts")]
            if v == "built":
                return [("Built — successes", snap.get("built_recent", []),
                         lambda b: "%-36s %-9s %9s  %s" % (b["slug"], b.get("backend", ""),
                         human(b.get("bytes", 0)), b.get("compare", "")),
                         lambda b: GREEN, "built")]
            if v == "failures":
                secs = []
                cats = snap.get("fail_categories", [])
                if cats:
                    secs.append(("Failures by cause", cats,
                                 lambda c: f"{c['count']:>4}  {c['cat']:<24} {c['hint']}",
                                 lambda c: YEL, "failcat"))
                secs.append(("Failures — newest first", snap.get("failures_recent", []),
                             lambda f: f"{f['slug']:<34} {f['error']}", lambda f: RED, "failures"))
                return secs
            if v == "stats":
                phases = sorted(snap.get("phase_durations", {}).items(), key=lambda kv: -kv[1])
                ops = sorted(snap.get("op_stats", {}).items(), key=lambda kv: -kv[1]["total"])
                return [("Phase timing", phases, lambda kv: f"{kv[0]:<12} {hms(kv[1])}",
                         lambda kv: 0, None),
                        ("Operation timing", ops,
                         lambda kv: f"{kv[0]:<10} total {kv[1]['total']:>9.1f}  n {kv[1]['count']:>5}"
                         f"  mean {kv[1]['mean']:>7.2f}  max {kv[1]['max']:>7.1f}",
                         lambda kv: CYAN, "stats")]
            return []

        mon = getattr(self, "monitor", False)
        setup = getattr(self, "setup", False)        # pre-build first-run Configuration screen
        view = "config"                              # land on the Configuration tab (leftmost)
        sel, detail, dscroll = 0, None, 0
        section_idx = 0                              # focused section in a multi-section tab
        cfg_fields = None                            # the editable Configuration fields
        cfg_active = 0                               # selected field, or the action button
        while True:
            if self.orch.stop.is_set() and not mon and not setup:
                break
            snap = self.orch.snapshot()
            if cfg_fields is None:                   # build the editable fields once, from config
                cfg_fields = self._cfg_init_fields(self._effective_config(snap))
            vals = self._cfg_typed(cfg_fields)
            vis = self._cfg_visible(cfg_fields, vals)
            actions = ["▶ Start build", "Cancel"] if setup else ["✓ apply changes"]
            nav_n = len(vis) + len(actions)           # visible fields + action button(s)
            cfg_active = max(0, min(cfg_active, nav_n - 1))
            af = vis[cfg_active] if cfg_active < len(vis) else None   # active field (None=action)
            af_editable = af is not None and (setup or af.get("live"))
            text_active = af_editable and af["type"] in ("path", "text")  # 'q'/'C' type, not quit

            ch = stdscr.getch()
            if ch == curses.KEY_RESIZE:               # terminal resized: refresh LINES/COLS so the
                try:                                  # next draw re-flows to the new size, then idle
                    curses.update_lines_cols()
                except (curses.error, AttributeError):
                    pass
            elif ch == 27:                            # Esc: close an open detail, else cancel/quit
                if detail is not None:                # (app-mode arrow keys decode to KEY_*, so a
                    detail = None                     #  bare 27 is a real Esc, not a split sequence)
                else:
                    if not mon and not setup:
                        self.orch.stop.set()
                    return None
            elif ch in (ord("q"), ord("Q")) and not text_active:
                if not mon and not setup:
                    self.orch.stop.set()
                return None
            elif ch == 9 and not setup:               # Tab → next tab (live only; setup = config only)
                view = self.VIEWS[(self.VIEWS.index(view) + 1) % len(self.VIEWS)]
                cfg_active = sel = section_idx = 0; detail = None
            elif ch == curses.KEY_BTAB and not setup:  # Shift-Tab → previous tab
                view = self.VIEWS[(self.VIEWS.index(view) - 1) % len(self.VIEWS)]
                cfg_active = sel = section_idx = 0; detail = None
            elif detail is not None:                  # inside the detail overlay
                if ch in (10, 13, curses.KEY_BACKSPACE, 127, 8, curses.KEY_LEFT):
                    detail = None                     # ↵ / ⌫ / ← all return to the list
                elif ch == curses.KEY_DOWN:
                    dscroll += 1
                elif ch == curses.KEY_UP:
                    dscroll = max(0, dscroll - 1)
            elif view == "config":                    # --- the unified Configuration editor ---
                if ch in (ord("c"), ord("C")) and not setup and not text_active:
                    return "reconfigure"              # live: restart into setup to change paths
                elif ch == curses.KEY_UP:
                    cfg_active = (cfg_active - 1) % nav_n
                elif ch == curses.KEY_DOWN:
                    cfg_active = (cfg_active + 1) % nav_n
                elif cfg_active >= len(vis):          # an action button
                    if ch in (10, 13, ord(" ")):
                        which = actions[cfg_active - len(vis)]
                        if which == "Cancel":
                            return None
                        if setup:
                            return self._cfg_typed(cfg_fields)     # main applies + launches
                        self._cfg_apply_live(cfg_fields, snap)     # write control.json
                elif af_editable:                     # edit the selected field (wizard-style)
                    if self._cfg_field_key(af, ch) == "advance":
                        cfg_active = (cfg_active + 1) % nav_n
            else:                                     # --- other tabs: sections + items ---
                secs = sections_for(snap, view)
                if ch in (ord("p"), ord("P")):
                    (self.orch.paused.clear if self.orch.paused.is_set() else self.orch.paused.set)()
                elif ch in (ord("c"), ord("C")) and not setup:
                    return "reconfigure"
                elif ch == curses.KEY_RIGHT and len(secs) > 1:   # → next section (Ctrl+Tab is
                    section_idx = (section_idx + 1) % len(secs); sel = 0   # eaten by most terminals)
                elif ch == curses.KEY_LEFT and len(secs) > 1:    # ← previous section
                    section_idx = (section_idx - 1) % len(secs); sel = 0
                elif ch == curses.KEY_DOWN:
                    sel += 1
                elif ch == curses.KEY_UP:
                    sel = max(0, sel - 1)
                elif ch in (10, 13):                  # ↵ act on the focused section's selection
                    si = min(section_idx, len(secs) - 1) if secs else 0
                    if secs:
                        _, items, _, _, dview = secs[si]
                        if items and dview:
                            detail = self._detail_lines(dview, items[min(sel, len(items) - 1)])
                            dscroll = 0

            snap = self.orch.snapshot()
            if view != "config":                      # clamp the focused section + its selection
                secs = sections_for(snap, view)
                section_idx = max(0, min(section_idx, len(secs) - 1)) if secs else 0
                foc_items = secs[section_idx][1] if secs else []
                sel = max(0, min(sel, len(foc_items) - 1)) if foc_items else 0
            c, bk = snap["counts"], snap["backends"]
            h, w = stdscr.getmaxyx()
            stdscr.erase()
            cfg_cursor = None                         # caret to show for an active text field

            # ---- focused item → the always-on status panel (1-3 lines, reserved above the footer) ----
            fkind, fitem, ftitle = None, None, ""
            if detail is None:
                if view == "config":
                    vals = self._cfg_typed(cfg_fields)
                    vis = self._cfg_visible(cfg_fields, vals)
                    actions = ["▶ Start build", "Cancel"] if setup else ["✓ apply changes"]
                    cfg_active = max(0, min(cfg_active, len(vis) + len(actions) - 1))
                    if cfg_active < len(vis):
                        fkind, fitem = "field", vis[cfg_active]
                    else:
                        fkind, fitem = "action", actions[cfg_active - len(vis)]
                else:
                    fsecs = sections_for(snap, view)
                    if fsecs:
                        ftitle, fitems, _ff, _fc, fkind = fsecs[min(section_idx, len(fsecs) - 1)]
                        if fitems:
                            fitem = fitems[min(sel, len(fitems) - 1)]
            info_lines = self._focus_info(fkind, fitem, ftitle)[:3] if fitem is not None else []
            panel_h = (1 + len(info_lines)) if info_lines else 0
            body_bottom = h - 1 - panel_h             # body renders in rows < body_bottom; footer h-1

            def put(y, x, s, attr=0):
                if 0 <= y < h and 0 <= x < w and w - x - 1 > 0:
                    try:
                        stdscr.addnstr(y, x, str(s), w - x - 1, attr)
                    except curses.error:
                        pass

            grand = snap["total"] or 1
            done = c["built"] + c["failed"]
            ph = snap["phase"]
            plabel = self.PHASE_LABEL.get(ph, ph)
            pre_build = bool(snap.get("pre_build")) or setup
            # header
            put(0, 0, " Google Fonts library build" + (" [PAUSED]" if snap["paused"] else ""),
                curses.A_BOLD)
            if pre_build:
                put(0, max(0, w - 18), "first-time setup", curses.A_DIM)
                put(1, 0, " configure your build below, then navigate to ▶ Start build", CYAN)
            else:
                put(0, max(0, w - 24), f"elapsed {hms(snap['elapsed'])}", curses.A_BOLD)
                put(1, 0, f" build dir {human(snap.get('disk_build_total', 0))}  "
                          f"free {human(snap['disk_free'])}  jobs {snap['jobs']}  "
                          f"cohorts {len(snap['cohorts'])}  fontc {bk['fontc']}/fontmake {bk['fontmake']}",
                    CYAN)
                # phase banner + progress
                if ph in ("archive", "cohorts") and snap["phase_total"]:
                    pd, pt = snap["phase_done"], snap["phase_total"]
                    put(2, 0, f" Phase: {plabel}  {pd}/{pt}  {snap['phase_label'][:30]}",
                        YEL | curses.A_BOLD)
                    frac = pd / max(1, pt)
                else:
                    put(2, 0, f" Phase: {plabel}   built {c['built']}/{grand}  failed {c['failed']}  "
                              f"building {c['building']}  queued {c['queued']}", curses.A_BOLD)
                    frac = done / grand
                if snap["phase_error"]:
                    put(2, max(0, w - 30), f"ERR {snap['phase_error'][:24]}", RED)
                barw = max(10, w - 4)
                fill = int(barw * frac)
                put(3, 1, "[" + "#" * fill + "-" * (barw - fill) + "]")
                put(3, max(2, barw // 2), f" {int(100 * frac)}% ", curses.A_BOLD)
            # tabs — Tab / Shift-Tab are the ONLY way to switch (←→ and numbers edit fields)
            x = 1
            for name in self.VIEWS:
                put(4, x, f" {name} ", curses.A_REVERSE if view == name else curses.A_DIM)
                x += len(name) + 3
            put(4, max(x + 2, w - 24), "[Tab]/[⇧Tab] switch tabs", curses.A_DIM)

            # ---- section renderer: all sections stacked; the FOCUSED one is highlighted and its
            # selected item reversed (↑↓ scroll it). Non-focused sections show a short peek. ----
            def draw_sections(row0, secs):
                # Plan the layout (provably fits in `avail`, focused selection always visible),
                # then render. Recomputed every frame from body_bottom, so it re-flows on resize.
                avail = max(0, body_bottom - row0)
                plan = self._layout_sections(avail, [len(s[1]) for s in secs], section_idx, sel)
                used = sum(1 + e["count"] + (1 if e["hint"] else 0) + (1 if e["none"] else 0)
                           for e in plan)
                slack = max(0, avail - used)               # spare rows → opportunistic blank gaps
                r = row0
                for e in plan:
                    si = e["idx"]
                    title, items, fmt, color, _dv = secs[si]
                    foc = (si == section_idx)
                    mark = "▼ " if foc else "▷ "
                    if r < body_bottom:
                        put(r, 0, (" " + mark + f"{title} ({len(items)}) ").ljust(w - 1, "-"),
                            curses.A_BOLD | (curses.A_REVERSE if foc else 0)); r += 1
                    if e["none"]:
                        if r < body_bottom:
                            put(r, 1, "(none)", curses.A_DIM); r += 1
                    else:
                        top = e["top"]
                        for idx in range(top, top + e["count"]):
                            if r >= body_bottom:
                                break
                            a = (color(items[idx]) or 0) | (curses.A_REVERSE if foc and idx == sel else 0)
                            put(r, 1, fmt(items[idx]), a); r += 1
                        if e["hint"] and r < body_bottom:
                            put(r, 1, f"  … (+{len(items) - e['count']} more)", curses.A_DIM)
                            r += 1
                    if slack > 0 and r < body_bottom:       # opportunistic blank separator
                        r += 1; slack -= 1

            row = 6
            # ---- completion / stopped banner: when the build has finished or the daemon is gone,
            # say so prominently (with the outcome + the top failure cause + what to do next) so the
            # dashboard never just looks frozen mid-flight. ----
            done_state = bool(snap.get("done")) or ph == "done"
            stopped = mon and not snap.get("daemon_alive", True)
            if (done_state or stopped) and detail is None and not pre_build and body_bottom - row >= 5:
                cc = snap["counts"]
                if done_state:
                    head = (f" ✓ BUILD COMPLETE — {cc['built']} built · {cc['failed']} failed · "
                            f"{cc.get('skipped', 0)} skipped  (of {snap['total']} selected)")
                    battr = GREEN | curses.A_BOLD | curses.A_REVERSE
                else:
                    head = (f" ⚠ BUILD STOPPED — daemon not running · {cc['built']} built · "
                            f"{cc['failed']} failed · {cc['queued']} still queued")
                    battr = YEL | curses.A_BOLD | curses.A_REVERSE
                put(row, 0, head.ljust(w - 1), battr); row += 1
                cats = snap.get("fail_categories", [])
                if cats and row < body_bottom:
                    t = cats[0]
                    put(row, 0, f"   top failure: {t['cat']} ({t['count']}) — {t['hint']}", YEL)
                    row += 1
                if row < body_bottom:
                    nxt = "   next: press [C] to reconfigure · re-run to continue"
                    if cc.get("skipped"):
                        nxt += " · raise % in config to include skipped"
                    put(row, 0, nxt, curses.A_DIM); row += 1
                row += 1                              # blank separator below the banner
            if detail is not None:                   # ---- detail overlay for the selected item ----
                put(5, 0, " Details — [Esc/←/↵] back   [↑↓] scroll ".ljust(w - 1, "-"), curses.A_BOLD)
                lines = detail                        # captured once when opened (no per-frame I/O)
                dscroll = min(dscroll, max(0, len(lines) - 1))
                for i, ln in enumerate(lines[dscroll:dscroll + max(1, h - row - 1)]):
                    put(row + i, 2, ln, curses.A_BOLD if ln and not ln.startswith(" ") and ln.endswith(":") else 0)
            elif view == "config":
                live_cfg = self._effective_config(snap)
                VC = 36                               # value column
                title = (" Configuration — set up your build "
                         if pre_build else " Configuration — edit settings (live where possible) ")
                put(row, 0, title.ljust(w - 1, "-"), curses.A_BOLD); row += 1
                # On a short terminal the fields would overflow into the reserved panel rows; scroll
                # them (keeping the active field visible) and reserve room for the action button.
                field_budget = max(1, body_bottom - row - 2)
                fstart = 0
                if len(vis) > field_budget:
                    afield = min(cfg_active, len(vis) - 1)
                    fstart = min(max(0, afield - field_budget + 1), len(vis) - field_budget)
                    if fstart > 0:
                        put(row, 1, f"  ↑ {fstart} more", curses.A_DIM); row += 1
                        field_budget -= 1
                for i in range(fstart, min(len(vis), fstart + field_budget)):
                    f = vis[i]
                    active = (cfg_active == i)
                    editable = setup or f.get("live")
                    if f["type"] == "bool":
                        valstr = "[x] yes" if f["value"] else "[ ] no"
                    elif f["type"] == "choice":
                        valstr = f"‹ {f['value']} ›"
                    else:
                        valstr = f["value"] or ""
                    tag = ""
                    if not pre_build:
                        if f.get("live") and vals.get(f["key"]) != live_cfg.get(f["key"]):
                            tag = "  *changed"
                        elif not f.get("live"):
                            tag = "  (restart: C)"
                    lab_attr = curses.A_BOLD if active else (0 if editable else curses.A_DIM)
                    put(row, 1, ("▸ " if active else "  ") + f["label"], lab_attr)
                    put(row, VC, valstr + tag,
                        (curses.A_REVERSE if active else 0) | (0 if editable else curses.A_DIM))
                    if active and editable and f["type"] not in ("bool", "choice"):
                        cfg_cursor = (row, VC + min(f.get("_caret", len(f["value"])), len(f["value"])))
                    row += 1
                if len(vis) > fstart + field_budget:
                    put(row, 1, f"  ↓ {len(vis) - fstart - field_budget} more", curses.A_DIM); row += 1
                # action button(s): ▶ Start build / Cancel (setup) or ✓ apply changes (live).
                # Clamp into the body so it never lands on the reserved panel rows (and is reachable).
                brow = min(row + 1, body_bottom - 1)
                bx = 2
                for ai, actlbl in enumerate(actions):
                    put(brow, bx, f" {actlbl} ",
                        curses.A_REVERSE if cfg_active == len(vis) + ai else curses.A_BOLD)
                    bx += len(actlbl) + 4
                row = brow + 2
                relax = snap.get("dep_relaxations", [])
                if relax and row < body_bottom - 1:
                    put(row, 0, " auto-fixed dependencies (no manual pinning needed) ".ljust(w - 1, "-"),
                        curses.A_BOLD); row += 1
                    for ln in relax[:max(0, (body_bottom - row) // 3)]:
                        put(row, 1, ln, YEL); row += 1
                clog = snap.get("control_log", [])
                if clog and row < body_bottom - 1:
                    put(row, 0, " applied live changes ".ljust(w - 1, "-"), curses.A_BOLD); row += 1
                    for ln in clog[-max(0, body_bottom - row - 1):]:
                        put(row, 1, ln, GREEN); row += 1
            elif view == "stats":                    # migration summary, then the timing sections
                m = snap.get("migration", {})
                put(row, 0, " fontc migration ".ljust(w - 1, "-"), curses.A_BOLD); row += 1
                put(row, 1, f"fontc {m.get('fontc', 0)}   fontmake-fallback(blockers) "
                            f"{m.get('fontmake_fallback', 0)}   fontmake-only {m.get('fontmake_only', 0)}"
                            + (f"   both id {m.get('both_identical', 0)}/diff {m.get('both_differ', 0)}"
                               if (m.get('both_identical') or m.get('both_differ')) else ""),
                    GREEN); row += 2
                draw_sections(row, sections_for(snap, "stats"))
            else:                                    # overview / cohorts / built / failures
                draw_sections(row, sections_for(snap, view))

            # ---- always-on status panel: what the focused item means (above the footer) ----
            if info_lines:
                sy = h - 1 - panel_h
                put(sy, 0, "─" * (w - 1), curses.A_DIM)
                for k, ln in enumerate(info_lines):
                    put(sy + 1 + k, 0, " " + ln, (CYAN | curses.A_BOLD) if k == 0 else CYAN)

            if detail is not None:
                foot = " [esc/←/↵] back to list   [↑↓] scroll"
            elif view == "config":
                foot = (" [↑↓]field  [←→]edit/step  [space]toggle  type to edit  [↵]▶ Start  [Esc]cancel"
                        if setup else
                        " [↑↓]field  [←→]edit  [space]toggle  [↵]apply  [C]restart for paths  [Tab]tabs")
            else:
                foot = " [Tab]tabs  [←→]section  [↑↓]item  [↵]details  [C]onfig  [q]uit"
            if mon and not snap.get("daemon_alive", True):
                foot = " [q]uit   ⚠ daemon not running (build finished or stopped)"
            put(h - 1, 0, foot, curses.A_DIM)
            try:                                      # show the text caret on an active field
                if cfg_cursor:
                    curses.curs_set(1)
                    stdscr.move(min(cfg_cursor[0], h - 1), min(cfg_cursor[1], w - 1))
                else:
                    curses.curs_set(0)
            except curses.error:
                pass
            stdscr.refresh()
            if snap["done"] and not mon and not setup:
                time.sleep(1.0)
                break
            time.sleep(0.25)


FRONTENDS = {"curses": CursesFrontend, "plain": PlainFrontend,
             "json": JsonFrontend, "none": NoneFrontend}


def pick_frontend(name: str) -> str:
    if name != "auto":
        return name
    if not sys.stdout.isatty():
        return "plain"
    try:
        import curses  # noqa: F401
        return "curses"
    except Exception:
        return "plain"


def run_monitor(build_dir: Path, ui: str):
    """Attach a read-only live monitor to a (possibly detached) build at build_dir.
    Returns "reconfigure" if the user pressed C to go back to the setup wizard."""
    fe = FRONTENDS[ui if ui in FRONTENDS else "plain"](MonitorState(build_dir))
    fe.monitor = True
    return fe.run()


def reexec_wizard():
    """Restart the program forcing the setup wizard (the dashboard's 'C' key). Re-exec is the
    simplest reliable way to return to the (one-shot, curses) wizard from the monitor; any
    running daemon is left alone and is only replaced if the user actually starts a new build.
    Drops flags that would bypass the wizard (--attach/--yes/-y) and forces --wizard."""
    drop = {"--attach", "--yes", "-y"}
    argv = [sys.executable] + [a for a in sys.argv if a not in drop]
    if "--wizard" not in argv:
        argv.append("--wizard")
    sys.stdout.flush()
    sys.stderr.flush()
    os.execv(sys.executable, argv)


def print_timing_summary(data: dict):
    e = sys.stderr
    print("\nTiming (per-phase + per-operation; full detail in timings.json):", file=e)
    for ph, sec in sorted(data.get("phases", {}).items(), key=lambda kv: -kv[1]):
        print(f"  phase {ph:<10} {hms(sec)}", file=e)
    for op, s in sorted(data.get("operations", {}).items(), key=lambda kv: -kv[1]["total"]):
        print(f"  op    {op:<10} total {s['total']:>9.1f}s  n {s['count']:>5}  "
              f"mean {s['mean']:>6.2f}s  max {s['max']:>6.1f}s", file=e)


def print_migration_summary(data: dict):
    e = sys.stderr
    s = data["summary"]
    print("\nfontc migration (full detail in migration.json):", file=e)
    print(f"  built with fontc                     : {s['fontc']}", file=e)
    print(f"  fontmake fallback (fontc FAILED) ⇐ blockers : {s['fontmake_fallback']}", file=e)
    print(f"  fontmake only (fontc not attempted)  : {s['fontmake_only']}", file=e)
    if s['both_identical'] or s['both_differ']:
        print(f"  both: identical {s['both_identical']}  differ {s['both_differ']}", file=e)
    for b in data.get("fontmake_fallback", [])[:10]:
        print(f"    blocker {b['slug']}: {b['fontc_error'][:80]}", file=e)
    extra = len(data.get("fontmake_fallback", [])) - 10
    if extra > 0:
        print(f"    … (+{extra} more blockers in migration.json)", file=e)


# =============================================================================== report

def cohorts_report(families: List[Family], archive: Path, jobs: int, out_path: Optional[Path],
                   context: str = ""):
    """Scan each family's requirements.txt (read-only, via git show on the mirror) and
    print the dependency-cohort grouping. No extraction, no builds, archives untouched."""
    from concurrent.futures import ThreadPoolExecutor
    from collections import defaultdict

    def work(fam: Family):
        mp = mirror_path(archive, fam.repo_url)
        if not mp.is_dir():
            return fam.slug, "(mirror-absent)", ""
        req = read_requirements_from_mirror(mp, fam.commit)
        return fam.slug, cohort_key_for(req), normalize_requirements(req)

    rows: Dict[str, Tuple[str, str]] = {}
    with ThreadPoolExecutor(max_workers=max(1, jobs)) as ex:
        for slug, cohort, sig in ex.map(work, families):
            rows[slug] = (cohort, sig)

    groups: Dict[str, List[str]] = defaultdict(list)
    sigs: Dict[str, str] = {}
    for slug, (cohort, sig) in rows.items():
        groups[cohort].append(slug)
        sigs.setdefault(cohort, sig)

    real = [k for k in groups if k not in ("base", "(mirror-absent)")]
    print(f"Cohort report: {len(families)} repos scanned{context} -> {len(real)} distinct "
          f"dependency cohort(s), plus 'base' (no requirements file) and any mirror-absent.\n")
    for cohort, slugs in sorted(groups.items(), key=lambda kv: -len(kv[1])):
        label = {"base": "base — no requirements file",
                 "(mirror-absent)": "mirror absent — not scanned"}.get(cohort, cohort)
        print(f"== {label}  ·  {len(slugs)} families ==")
        pkgs = sigs.get(cohort, "").splitlines()
        if pkgs:
            print("   deps: " + ", ".join(pkgs[:8]) + (f"  (+{len(pkgs) - 8} more)" if len(pkgs) > 8 else ""))
        shown = sorted(slugs)[:12]
        print("   " + ", ".join(shown) + (f"  … (+{len(slugs) - 12} more)" if len(slugs) > 12 else ""))
        print()
    if out_path:
        data = {"total": len(families), "distinct_cohorts": len(real),
                "cohorts": {c: {"count": len(s), "requirements": sigs.get(c, ""),
                                "families": sorted(s)} for c, s in groups.items()}}
        out_path.write_text(json.dumps(data, indent=1))
        print(f"(full JSON written to {out_path})", file=sys.stderr)


# ================================================================================= main

# =============================================== config persistence / daemon / monitor

CONFIG_KEYS = ("source", "google_fonts", "archive", "build_dir", "backend", "fontc_bin",
               "jobs", "percent", "timeout", "populate_archive", "manage_venvs",
               "retry_failed", "base_requirements", "compare", "data_dir")


def load_config(path: Path) -> dict:
    try:
        d = json.loads(path.read_text())
        return {k: v for k, v in d.items() if k in CONFIG_KEYS}
    except Exception:
        return {}


def save_config(path: Path, args) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        data = {k: getattr(args, k, None) for k in CONFIG_KEYS}
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=1))
        tmp.replace(path)
    except OSError:
        pass


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def _proc_cmdline(pid: int) -> str:
    try:
        return Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\x00", b" ").decode("utf-8", "replace")
    except OSError:
        try:
            return subprocess.run(["ps", "-p", str(pid), "-o", "args="],
                                  capture_output=True, text=True, timeout=5).stdout
        except Exception:
            return ""


def write_control(build_dir: Path, settings: dict) -> bool:
    """Bump the control.json the running daemon polls, to apply a live config change. Returns
    True on success. Uses a unique temp name so two monitors writing at once can't collide on
    the temp file (the seq is still last-writer-wins — drive config from one monitor at a time)."""
    path = Path(build_dir) / "control.json"
    try:
        seq = int(json.loads(path.read_text()).get("seq", 0)) if path.is_file() else 0
        tmp = path.with_suffix(f".{os.getpid()}.tmp")
        tmp.write_text(json.dumps({"seq": seq + 1, "set": settings}))
        tmp.replace(path)
        return True
    except OSError:
        return False


def reset_build(build_dir: Path, archive: Path, google_fonts, assume_yes: bool) -> None:
    """Delete ALL built assets + venvs (the whole build dir) to start clean. The repo archive
    is NEVER deleted (strict append-only policy) — we refuse if it lives inside the build dir."""
    bd = Path(build_dir).resolve()
    ar = Path(archive).resolve()
    if bd == ar or bd in ar.parents:                  # archive is under (or is) the build dir
        sys.exit(f"refusing to reset: the archive {ar} is inside the build dir {bd}. The archive "
                 f"is append-only and must never be deleted — put --archive outside --build-dir.")
    if read_daemon_pid(bd) is not None:
        sys.exit(f"a build is running at {bd}; --stop it first, then --reset.")
    if not bd.exists():
        print(f"nothing to reset — {bd} does not exist (archive untouched).", file=sys.stderr)
        return
    e = sys.stderr
    print("RESET will DELETE the build dir (built fonts, venvs, work, caches, logs, state):", file=e)
    print(f"  {bd}", file=e)
    print("KEPT, never touched:", file=e)
    print(f"  archive       {ar}", file=e)
    if google_fonts:
        print(f"  google/fonts  {Path(google_fonts).resolve()}", file=e)
    if not assume_yes:
        if not sys.stdin.isatty():
            sys.exit("refusing to reset non-interactively — pass --yes to confirm.")
        try:
            if input("Delete the build dir? [y/N] ").strip().lower() not in ("y", "yes"):
                print("aborted — nothing deleted.", file=e)
                return
        except (EOFError, KeyboardInterrupt):
            print("\naborted — nothing deleted.", file=e)
            return
    shutil.rmtree(bd, ignore_errors=True)
    print(f"reset done — deleted {bd}. The archive is intact.", file=e)


def read_daemon_pid(build_dir: Path) -> Optional[int]:
    try:
        pid = int((Path(build_dir) / "daemon.pid").read_text().strip())
    except (OSError, ValueError):
        return None
    if not _pid_alive(pid):
        return None
    cl = _proc_cmdline(pid)                 # guard against PID reuse by an unrelated process
    if cl and "gflib_build" not in cl and "gflib-build" not in cl:
        return None
    return pid


def daemonize(build_dir: Path) -> bool:
    """Double-fork. Returns True in the detached daemon (which should run the build), and
    False in the original parent (which can then attach a monitor). Writes daemon.pid and
    redirects the daemon's stdio to daemon.log."""
    pid = os.fork()
    if pid > 0:                       # original parent
        os.waitpid(pid, 0)            # reap the short-lived first child
        return False
    os.setsid()                       # first child
    if os.fork() > 0:
        os._exit(0)
    sys.stdout.flush(); sys.stderr.flush()   # grandchild = daemon
    log = open(Path(build_dir) / "daemon.log", "a", buffering=1)
    nul = open(os.devnull, "r")
    os.dup2(nul.fileno(), sys.stdin.fileno())
    os.dup2(log.fileno(), sys.stdout.fileno())
    os.dup2(log.fileno(), sys.stderr.fileno())
    (Path(build_dir) / "daemon.pid").write_text(str(os.getpid()))
    return True


class MonitorState:
    """Read-only view of a (possibly detached) build for the monitor UI. Mimics the slice of
    the Orchestrator interface the frontends use (snapshot/stop/paused/build_dir/workers)."""
    _EMPTY = {"phase": "(waiting for build…)",
              "counts": {"built": 0, "failed": 0, "building": 0, "queued": 0, "skipped": 0},
              "backends": {"fontc": 0, "fontmake": 0}, "building": [], "failures_recent": [],
              "built_recent": [], "fail_categories": [],
              "cohorts": [], "total": 0, "elapsed": 0, "disk_used_delta": 0, "disk_build_total": 0,
              "disk_free": 0,
              "jobs": 0, "paused": False, "phase_total": 0, "phase_done": 0, "phase_label": "",
              "phase_error": "", "op_stats": {}, "phase_durations": {}, "migration": {},
              "tasks": [], "archive_recent": [], "config": {}, "control_log": [],
              "dep_relaxations": [], "done": False}

    def __init__(self, build_dir: Path):
        self.build_dir = Path(build_dir)
        self.stop = threading.Event()
        self.paused = threading.Event()
        self.lock = threading.Lock()
        self.workers: List = []
        self.results: Dict = {}

    def snapshot(self) -> dict:
        try:
            snap = json.loads((self.build_dir / "status.json").read_text())
        except Exception:
            snap = dict(self._EMPTY)
        snap["daemon_alive"] = read_daemon_pid(self.build_dir) is not None
        return snap

    def all_done(self) -> bool:
        return self.snapshot().get("done", False)


class SetupState:
    """Pre-build state: the dashboard runs on the Configuration tab to set up a NEW build (the
    first-run / reconfigure entry). It just holds the initial config; the config tab edits it
    and returns the chosen settings to main() on ▶ Start."""
    def __init__(self, config: dict, build_dir, cfg_path):
        self.build_dir = Path(build_dir)
        self.stop = threading.Event()
        self.paused = threading.Event()
        self.lock = threading.Lock()
        self.workers: List = []
        self.results: Dict = {}
        self._config = dict(config)
        self._cfg_path = str(cfg_path or "")

    def snapshot(self) -> dict:
        s = dict(MonitorState._EMPTY)
        s.update({"config": dict(self._config), "config_path": self._cfg_path,
                  "pre_build": True, "phase": "config", "daemon_alive": True})
        return s

    def all_done(self) -> bool:
        return False




def config_screen_plain(steps, gf_path, archive, build_dir, args) -> bool:
    """Fallback plain-terminal confirmation (used if curses is unavailable)."""
    e = sys.stderr
    print("\n=== gflib-build configuration ===", file=e)
    for i, (title, detail) in enumerate(steps, 1):
        print(f"  {i}. {title}: {detail}", file=e)
    if gf_path:
        print(f"  google/fonts : {gf_path}", file=e)
    print(f"  archive      : {archive}\n  build dir    : {build_dir}", file=e)
    try:
        return input("Proceed? [y/N] ").strip().lower() in ("y", "yes")
    except (EOFError, KeyboardInterrupt):
        return False


def build_argparser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        description="From-scratch, archive-safe, Rust-first full-library build of Google Fonts.")
    ap.add_argument("--source", choices=["metadata", "archive"], default="metadata",
                    help="where the worklist/stats come from: 'metadata' = parse google/fonts "
                         "METADATA.pb (pinned commits + configs + shipped refs); 'archive' = every "
                         "bare mirror in the archive at --archive-rev (google/fonts optional)")
    ap.add_argument("--data-dir", default="gflib-data",
                    help="root for default paths (google-fonts/, archive/, build/) when those "
                         "are not given explicitly")
    ap.add_argument("--google-fonts", default=None,
                    help="path to a google/fonts clone (default <data-dir>/google-fonts; cloned if "
                         "absent in metadata mode). Required-ish for --source metadata and --compare")
    ap.add_argument("--archive", default=None,
                    help="repo archive of bare mirrors (default <data-dir>/archive)")
    ap.add_argument("--archive-rev", default="HEAD",
                    help="revision to build for --source archive (default HEAD = default-branch tip)")
    ap.add_argument("--build-dir", default=None, help="output dir (default <data-dir>/build; NOT in a repo)")
    ap.add_argument("--populate-archive", dest="populate_archive", action="store_true", default=None,
                    help="mirror any missing upstream repos into the archive before building "
                         "(default ON for a metadata bootstrap; append-only)")
    ap.add_argument("--no-populate-archive", dest="populate_archive", action="store_false",
                    help="do not pre-populate the archive (missing repos fail unless --mirror-missing)")
    ap.add_argument("--yes", "-y", action="store_true",
                    help="skip the Configuration screen (non-interactive bootstrap with current settings)")
    ap.add_argument("--wizard", action="store_true",
                    help="always show the Configuration screen (even when nothing needs bootstrapping)")
    ap.add_argument("--backend", choices=["auto", "fontc", "fontmake", "both"], default="auto",
                    help="auto = fontc first, fall back to fontmake (the migration default); "
                         "fontc/fontmake = that compiler only; both = build with each and compare "
                         "outputs (fontc_crater-style)")
    ap.add_argument("--fontc-bin", default=None, help="path to the fontc (Rust) binary")
    ap.add_argument("--build-python", default=sys.executable,
                    help="interpreter for builds when --manage-venvs is off")
    ap.add_argument("--manage-venvs", dest="manage_venvs", action="store_true",
                    help="create & share one venv per dependency cohort")
    ap.add_argument("--no-manage-venvs", dest="manage_venvs", action="store_false",
                    help="disable cohort venvs (override a persisted setting)")
    ap.add_argument("--base-python", default=sys.executable, help="python used to create cohort venvs")
    ap.add_argument("--base-requirements", default=None, help="pinned base toolchain requirements file")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--timeout", type=int, default=None,
                    help="per-build timeout in seconds (default: no timeout — stop manually via the UI)")
    ap.add_argument("--percent", type=float, default=100.0,
                    help="build only this %% of the library (evenly-spaced sample) for validation")
    ap.add_argument("--only", default="", help="comma-separated slugs (e.g. ofl/dmsans)")
    ap.add_argument("--compare", dest="compare", action="store_true",
                    help="sha256-compare built fonts to shipped")
    ap.add_argument("--no-compare", dest="compare", action="store_false",
                    help="disable comparison (override a persisted setting)")
    ap.add_argument("--mirror-missing", action="store_true",
                    help="clone absent upstream repos into the archive (append-only)")
    ap.add_argument("--retry-failed", action="store_true")
    ap.add_argument("--rebuild", action="store_true")
    ap.add_argument("--keep-work", action="store_true", help="keep throwaway extractions")
    ap.add_argument("--keep-fonts", dest="keep_fonts", action="store_true", default=True)
    ap.add_argument("--discard-fonts", dest="keep_fonts", action="store_false")
    ap.add_argument("--ui", choices=["auto", "curses", "plain", "json", "none"], default="auto",
                    help="frontend (ncurses is optional; plain/json/none for other tooling)")
    ap.add_argument("--list", action="store_true", help="print the buildable worklist and exit")
    ap.add_argument("--cohorts-report", action="store_true",
                    help="scan each family's requirements.txt (read-only) and print the "
                         "dependency-cohort grouping, then exit (no builds)")
    ap.add_argument("--config", default=None,
                    help="settings config file (default <data-dir>/gflib-build.config); loaded as "
                         "defaults and updated with the chosen settings after each run")
    ap.add_argument("--no-save-config", dest="save_config", action="store_false", default=True,
                    help="do not persist the chosen settings to the config file")
    ap.add_argument("--detach", action="store_true",
                    help="run the build in a detached background daemon, then attach a live monitor "
                         "(quit the monitor with q and the build keeps running)")
    ap.add_argument("--attach", action="store_true",
                    help="attach a live, read-only monitor to a build at --build-dir (q leaves it running)")
    ap.add_argument("--stop", action="store_true",
                    help="signal a detached build at --build-dir to stop gracefully")
    ap.add_argument("--reset", action="store_true",
                    help="delete ALL built assets and virtual environments (the whole build dir) "
                         "to start clean. The repo archive is NEVER touched (append-only policy).")
    ap.set_defaults(manage_venvs=False, compare=False)   # pin defaults (overridden by config/CLI)
    return ap




def main():
    # ---- load persisted config as defaults (CLI flags still override) ----
    pre = argparse.ArgumentParser(add_help=False)
    pre.add_argument("--data-dir", default="gflib-data")
    pre.add_argument("--build-dir", default=None)
    pre.add_argument("--config", default=None)
    known, _ = pre.parse_known_args()
    cfg_path = Path(known.config) if known.config else Path(known.data_dir) / "gflib-build.config"
    cfg = load_config(cfg_path)
    ap = build_argparser()
    if cfg:
        ap.set_defaults(**{k: v for k, v in cfg.items() if k != "data_dir"})
    args = ap.parse_args()
    args._cfg_path = str(cfg_path)                # so the config tab can fall back to it
    if sys.platform == "win32":
        sys.exit("gflib-build targets macOS/Linux (POSIX venv layout, git archive, tar).")

    # ---- attach / stop an existing (possibly detached) build, then exit ----
    mon_build_dir = (Path(args.build_dir) if args.build_dir
                     else (Path(args.data_dir) / "build")).resolve()
    if args.stop:
        pid = read_daemon_pid(mon_build_dir)
        if pid is None:
            sys.exit(f"no running build daemon at {mon_build_dir}")
        os.kill(pid, signal.SIGTERM)
        print(f"sent stop to build daemon {pid} at {mon_build_dir}", file=sys.stderr)
        return
    if args.attach:
        if pick_frontend(args.ui) == "curses" and not sys.stdout.isatty():
            sys.exit("--attach needs a terminal")
        if run_monitor(mon_build_dir, pick_frontend(args.ui)) == "reconfigure":
            reexec_wizard()                      # C: drop --attach, re-exec into the wizard
        return

    # ---- resolve paths (defaults from --data-dir; auto-detect where possible) ----
    # All paths are made ABSOLUTE: the per-build subprocess runs with cwd=<extraction dir>,
    # so a relative build dir would make the venv interpreter / config paths unresolvable.
    data_dir = Path(args.data_dir).resolve()
    gf = (Path(args.google_fonts) if args.google_fonts
          else (data_dir / "google-fonts" if args.source == "metadata" else None))
    gf = gf.resolve() if gf else None
    if args.archive:
        archive = Path(args.archive)
    else:
        det = detect_archive(data_dir)        # auto-detect a pre-existing archive
        archive = Path(det) if det else (data_dir / "archive")
    archive = archive.resolve()
    build_dir = (Path(args.build_dir) if args.build_dir else (data_dir / "build")).resolve()

    if args.reset:                            # wipe built assets + venvs; keep the archive
        reset_build(build_dir, archive, gf, args.yes)
        return

    if not args.fontc_bin:
        args.fontc_bin = detect_fontc()       # auto-detect a fontc binary
    read_only = args.list or args.cohorts_report
    if args.populate_archive is None:
        args.populate_archive = (args.source == "metadata") and not read_only

    # ---- if a build is ALREADY RUNNING in this build_dir, just reattach a live monitor ----
    # (resume straight to live updates — no wizard — from this or any other terminal; q leaves
    #  it running, --stop cancels). Prevents accidentally starting a second build in one dir.
    # `--wizard` (incl. the dashboard's C re-exec) skips this so the wizard always shows.
    if not read_only and not args.wizard and read_daemon_pid(build_dir) is not None:
        ui = pick_frontend(args.ui)
        if ui == "curses" and not sys.stdout.isatty():
            sys.exit(f"a build is running at {build_dir}; attach from a terminal or use --attach")
        print(f"a build is already running at {build_dir} — reattaching live monitor "
              f"(q leaves it running; C reconfigures; --stop to cancel).", file=sys.stderr)
        if run_monitor(build_dir, ui) == "reconfigure":
            reexec_wizard()
        return

    need_gf_clone = args.source == "metadata" and not (gf and (gf / "ofl").is_dir())
    want_build_fontc = False
    steps = []
    if need_gf_clone:
        steps.append(("clone google/fonts", f"{GOOGLE_FONTS_URL} → {gf} (shallow)"))
    if args.populate_archive:
        steps.append(("populate archive", f"mirror missing upstream repos into {archive}"))

    # ---- first-run setup IS the Configuration tab: open the dashboard pre-build, fully
    #      editable, and launch on ▶ Start (no separate "wizard") ----
    if (steps or args.wizard) and not args.yes and not read_only:
        if not sys.stdin.isatty():
            sys.exit("missing prerequisites (google/fonts clone and/or archive). Re-run with "
                     "--yes to bootstrap non-interactively, or pass --google-fonts/--archive.")
        init_cfg = {"source": args.source, "google_fonts": str(gf) if gf else "",
                    "archive": str(archive), "build_dir": str(build_dir), "backend": args.backend,
                    "fontc_bin": args.fontc_bin or "", "jobs": args.jobs, "percent": args.percent,
                    "timeout": args.timeout, "populate_archive": bool(args.populate_archive),
                    "manage_venvs": bool(args.manage_venvs), "compare": bool(args.compare),
                    "retry_failed": bool(args.retry_failed)}
        fe = CursesFrontend(SetupState(init_cfg, build_dir, cfg_path))
        fe.setup = True
        try:
            edited = fe.run()                           # typed config dict (▶ Start) or None
        except Exception:                               # curses unusable → plain confirm
            edited = {} if config_screen_plain(steps, gf, archive, build_dir, args) else None
        if edited is None:
            sys.exit("aborted.")
        if isinstance(edited, dict) and edited:         # apply the chosen settings
            args.source = edited["source"]
            gf = Path(edited["google_fonts"]) if edited.get("google_fonts") else None
            archive = Path(edited["archive"]) if edited.get("archive") else archive
            build_dir = Path(edited["build_dir"]) if edited.get("build_dir") else build_dir
            args.backend = edited["backend"]
            args.fontc_bin = edited.get("fontc_bin") or None
            want_build_fontc = bool(edited.get("build_fontc")) and args.backend != "fontmake"
            args.jobs = max(1, int(edited["jobs"]))
            args.percent = edited["percent"]
            args.timeout = edited.get("timeout")        # already None when 0
            args.populate_archive = edited["populate_archive"]
            args.manage_venvs = edited["manage_venvs"]
            args.retry_failed = bool(edited.get("retry_failed", args.retry_failed))
            args.compare = edited.get("compare", False) and args.source == "metadata"
            need_gf_clone = args.source == "metadata" and not (gf and (gf / "ofl").is_dir())

    # ---- auto-default base requirements to the bundled file (so cohort venvs just work) ----
    if args.manage_venvs and not args.base_requirements:
        bundled = Path(__file__).resolve().parent / "requirements-build.txt"
        if bundled.is_file():
            args.base_requirements = str(bundled)

    # ---- finalize + validate FIRST — before any expensive clone/build (fail fast) ----
    gf = gf.resolve() if gf else None         # re-resolve (the wizard may have set relatives)
    archive = archive.resolve()
    build_dir = build_dir.resolve()
    if args.fontc_bin:
        args.fontc_bin = str(Path(args.fontc_bin).resolve())
    args.data_dir = str(data_dir)
    args.google_fonts = str(gf) if gf else None
    args.archive = str(archive)
    args.build_dir = str(build_dir)
    if not (0 < args.percent <= 100):
        sys.exit("percent must be in (0, 100]")
    if args.backend == "fontc" and not args.fontc_bin and not want_build_fontc:
        sys.exit("backend 'fontc' needs a fontc binary — set a path or enable 'build fontc "
                 "from source' (or use --backend auto, which falls back to fontmake)")
    if args.manage_venvs and not args.base_requirements:
        sys.exit("cohort venvs need a base requirements file and no bundled requirements-build.txt "
                 "was found next to the script; pass --base-requirements or use --no-manage-venvs")
    if want_build_fontc and not args.fontc_bin and detect_cargo() is None:
        sys.exit("'build fontc from source' needs cargo (Rust). " + RUST_INSTALL_HINT)
    if args.compare and args.source != "metadata":
        sys.exit("--compare requires --source metadata (it diffs against the shipped binaries)")
    if args.compare and gf is None:
        sys.exit("--compare needs a google/fonts clone")
    if args.source == "metadata" and gf is None:
        sys.exit("--source metadata needs a google/fonts clone")

    read_only = args.list or args.cohorts_report
    if args.save_config and not read_only:
        save_config(cfg_path, args)               # persist the chosen settings for next time

    archive.mkdir(parents=True, exist_ok=True)
    build_dir.mkdir(parents=True, exist_ok=True)
    for sub in ("work", "out", "logs"):
        (build_dir / sub).mkdir(exist_ok=True)

    # ---- read-only paths (--list / --cohorts-report) discover synchronously (no live UI) ----
    if read_only:
        if need_gf_clone:
            try:
                ensure_google_fonts(gf, on_progress=lambda m: print(f"  {m}", file=sys.stderr))
            except (RuntimeError, ValueError) as e:
                sys.exit(str(e))
        if args.source == "metadata" and not (gf / "ofl").is_dir():
            sys.exit(f"google/fonts {gf} is not a clone (no ofl/)")
        if not archive.is_dir():
            sys.exit(f"archive {archive} not available")
        if args.source == "archive":
            families, total, skipped = discover_from_archive(Path(args.archive), args.archive_rev, args.jobs)
        else:
            families, total, skipped = discover(gf)
        sampled = sample_evenly(families, args.percent)
        if args.source == "metadata":
            ctx = f" (of {total} total in the library; {skipped} not buildable: no config/commit)"
        else:
            ctx = (f" (of {total} mirrors in the archive; {skipped} with no resolvable rev "
                   f"at {args.archive_rev})")
        if args.list:
            for f in sampled:
                cfg = "override" if f.has_override else (f.config_yaml or "(auto)")
                print(f"{f.slug:<40} {cfg:<26} {f.repo_url}")
            print(f"\n{len(sampled)} selected ({args.percent:g}% of {len(families)} via "
                  f"--source {args.source}){ctx}", file=sys.stderr)
            return
        sel = sampled                              # --cohorts-report
        if args.only:
            keep = set(args.only.split(","))
            sel = [f for f in sampled if f.slug in keep]
        cohorts_report(sel, Path(args.archive), args.jobs, Path(args.build_dir) / "cohorts.json",
                       context=ctx)
        return

    # ---- build path: clone google/fonts, build fontc, discover, populate archive, cohorts and
    #      the builds ALL run live inside the driver, rendered as a task-list in the UI ----
    args._want_build_fontc = want_build_fontc
    args._data_dir = str(data_dir)

    ui = pick_frontend(args.ui)
    # A fresh interactive (curses) build runs detached by default: quitting the UI (q) frees the
    # shell while the build keeps running, and re-running reattaches a live monitor. plain/json/
    # none stay in the foreground for scripting/logging; --detach forces detach for any UI.
    detach = args.detach or ui == "curses"

    # if a previous build is still running here (e.g. the user pressed C to reconfigure and is
    # now starting a new one), stop it first so the two don't clobber the same build_dir
    old = read_daemon_pid(build_dir)
    if old:
        print(f"stopping the previous build daemon ({old}) at {build_dir}…", file=sys.stderr)
        try:
            os.kill(old, signal.SIGTERM)
        except OSError:
            pass
        for _ in range(60):
            if read_daemon_pid(build_dir) is None:
                break
            time.sleep(0.1)

    orch = Orchestrator(args)
    print(f"Starting build pipeline (backend={args.backend}); the live UI shows clone / fontc / "
          f"discover / archive / cohorts / build as a task-list.", file=sys.stderr)

    # register handlers BEFORE daemonize so the daemon inherits them (no kill-before-handler window)
    signal.signal(signal.SIGINT, lambda *_: orch.stop.set())
    signal.signal(signal.SIGTERM, lambda *_: orch.stop.set())

    if detach:
        if daemonize(build_dir):                  # detached daemon: run the build, headless
            orch.run()
            orch.join()
            orch.save_state()
            return
        # original parent: drop our copy of the daemon's events file, then attach a monitor
        orch._close_events()
        print(f"build running detached at {build_dir}. reattach any time by re-running, or:\n"
              f"  python3 {Path(sys.argv[0]).name} --attach --build-dir {build_dir}\n"
              f"  python3 {Path(sys.argv[0]).name} --stop   --build-dir {build_dir}",
              file=sys.stderr)
        time.sleep(0.5)
        if ui != "none":
            if run_monitor(build_dir, ui) == "reconfigure":   # C: back to the wizard
                reexec_wizard()
        return

    orch.run()
    frontend = FRONTENDS[ui](orch)
    action = None
    try:
        action = frontend.run()
        if action != "reconfigure":
            orch.join()
    finally:
        orch.save_state()
    if action == "reconfigure":                       # C in a foreground UI: back to the wizard
        reexec_wizard()

    c = orch.snapshot()["counts"]
    print(f"\nDONE: built {c['built']}, failed {c['failed']}. "
          f"state: {orch.state_path}  events: {orch.build_dir / 'events.jsonl'}", file=sys.stderr)
    print_migration_summary(orch.migration_report())
    print_timing_summary(orch.write_timings())


if __name__ == "__main__":
    main()
