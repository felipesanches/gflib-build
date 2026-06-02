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

    def dur(self) -> float:
        if self.started == 0:
            return 0.0
        return (self.ended or time.time()) - self.started


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


def ensure_mirror(archive: Path, repo_url: str, commit: str, mirror_missing: bool):
    mp = mirror_path(archive, repo_url)
    if not mp.is_dir():
        if not mirror_missing:
            return None, f"mirror absent: {mp.name} (use --mirror-missing)"
        mp.parent.mkdir(parents=True, exist_ok=True)
        rc, _, err = git(["clone", "--mirror", repo_url, str(mp)], timeout=1800)
        if rc != 0:
            tail = err.strip().splitlines()[-1] if err.strip() else str(rc)
            return None, f"mirror clone failed: {tail}"
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


def populate_archive(repo_urls, archive: Path, jobs: int,
                     on_progress: Optional[Callable[[int, int, str], None]] = None,
                     stop: "Optional[threading.Event]" = None):
    """Ensure every repo_url has a bare mirror in the archive; clone --mirror the missing
    ones (APPEND-ONLY — existing mirrors are skipped read-only and NEVER modified/deleted).
    Returns (added, failed, present). Parallel across `jobs`; aborts promptly if `stop` set."""
    from concurrent.futures import ThreadPoolExecutor
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
        mp.parent.mkdir(parents=True, exist_ok=True)
        rc, _, err = git(["clone", "--mirror", url, str(mp)], timeout=1800)
        if rc != 0:
            tail = err.strip().splitlines()[-1] if err.strip() else str(rc)
            return ("failed", url, tail)
        return ("added", url, "")

    with ThreadPoolExecutor(max_workers=max(1, jobs)) as ex:
        for status, url, msg in ex.map(one, urls):
            with lock:
                done[0] += 1
                if status == "added":
                    added.append(url)
                elif status == "failed":
                    failed.append((url, msg))
                else:
                    present += 1
                if on_progress:
                    on_progress(done[0], len(urls), url)
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
        log = self.root / f"{key}.install.log"
        if py.exists():
            return str(py), ""
        rc = subprocess.run([self.base_python, "-m", "venv", str(vdir)],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if rc.returncode != 0:
            return "", f"venv create rc={rc.returncode}: {rc.stdout.decode('utf-8','replace')[:200]}"
        install = [str(py), "-m", "pip", "install", "--disable-pip-version-check",
                   "--cache-dir", str(self.pip_cache)]
        if self.base_req and self.base_req.is_file():
            install += ["-r", str(self.base_req)]
        if key != "base":
            req_path = vdir / "cohort-requirements.txt"
            req_path.write_text(req_text)
            install += ["-r", str(req_path)]
        with open(log, "wb") as lf:
            p = subprocess.run(install, stdout=lf, stderr=subprocess.STDOUT)
        if p.returncode != 0:
            return "", f"pip install rc={p.returncode} (see {log.name})"
        return str(py), ""


# ============================================================================= building

def run_builder(python: str, config_path: Path, work: Path, log_path: Path,
                timeout: Optional[int], backend: str, fontc_bin: Optional[str]):
    """`timeout=None` means the build never times out (the user can stop it via the UI)."""
    env = dict(os.environ)
    env["SOURCE_DATE_EPOCH"] = "0"
    # gftools.builder shells out to fontmake / ninja / gftools / ttfautohint BY NAME, so
    # the chosen interpreter's bin/ must be on PATH (running venv/bin/python does not, by
    # itself, activate the venv).
    bindir = os.path.dirname(os.path.abspath(python))
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

    def __init__(self, args, families: List[Family], total: int, skipped: int):
        self.args = args
        self.build_dir = Path(args.build_dir)
        self.google_fonts = Path(args.google_fonts) if args.google_fonts else None
        self.archive = Path(args.archive)
        self.families = {f.slug: f for f in families}
        self.total_with_source = total
        self.skipped_no_config = skipped

        self.lock = threading.Lock()
        self.results: Dict[str, Result] = {}
        self.q: "queue.Queue[str]" = queue.Queue()
        self.stop = threading.Event()
        self.paused = threading.Event()
        self.start_time = time.time()
        self.disk_baseline = self._disk_used()
        self.failures: List[str] = []
        self.workers: List[threading.Thread] = []
        self._events = open(self.build_dir / "events.jsonl", "a", buffering=1)
        self._events_lock = threading.Lock()
        self._events_closed = False

        self.venvs: Optional[VenvManager] = None
        if args.manage_venvs:
            self.venvs = VenvManager(self.build_dir, args.base_python,
                                     Path(args.base_requirements) if args.base_requirements else None)

        # phase pipeline state (archive → cohorts → build → done)
        self.phase = "init"
        self.phase_total = 0
        self.phase_done = 0
        self.phase_label = ""
        self.phase_error = ""
        self.cohorts: Dict[str, dict] = {}
        self.driver: Optional[threading.Thread] = None
        # timing instrumentation (every operation is measured, for bottleneck analysis)
        self.op_stats: Dict[str, List[float]] = {}   # op -> [total_seconds, count, max]
        self.phase_durations: Dict[str, float] = {}
        self._phase_t0: Optional[float] = None
        self._status_thread: Optional[threading.Thread] = None
        self._status_stop = threading.Event()
        self._status_lock = threading.Lock()

        self._load_state()
        self._enqueue()

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
            except Exception:
                pass

    def save_state(self):
        with self.lock:
            data = {"saved_at": time.time(), "build_dir": str(self.build_dir),
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
        todo = []
        for slug, fam in self.families.items():
            if only and slug not in only:
                continue
            prev = self.results.get(slug)
            if prev and not self.args.rebuild:
                if prev.status == "built":
                    continue
                if prev.status == "failed" and not self.args.retry_failed:
                    continue
            todo.append(slug)

        def weight(slug):
            prev = self.results.get(slug)
            if prev and prev.ended > prev.started:        # resume: longest prior build first
                return prev.dur()
            fam = self.families[slug]                     # first run: heuristic
            return (1000 if fam.is_variable else 0) + len(fam.shipped_fonts)
        todo.sort(key=weight, reverse=True)

        for slug in todo:
            self.results[slug] = Result(slug=slug, status="queued")
            self.q.put(slug)

    # ---- read-only views
    def _disk_used(self) -> int:
        try:
            return shutil.disk_usage(self.build_dir).used
        except OSError:
            return 0

    def snapshot(self) -> dict:
        with self.lock:
            rs = list(self.results.values())
            counts = {"built": 0, "failed": 0, "building": 0, "queued": 0, "skipped": 0}
            backends = {"fontc": 0, "fontmake": 0}
            building = []
            for r in rs:
                counts[r.status] = counts.get(r.status, 0) + 1
                if r.status == "built" and r.backend:
                    backends[r.backend] = backends.get(r.backend, 0) + 1
                if r.status == "building":
                    building.append({"slug": r.slug, "worker": r.worker, "dur": r.dur(),
                                     "backend": r.backend, "note": r.note})
            building.sort(key=lambda b: -b["dur"])
            fails = [{"slug": s, "error": self.results[s].error, "log": self.results[s].log}
                     for s in self.failures[-50:] if s in self.results][::-1]
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
        return {
            "elapsed": time.time() - self.start_time,
            "disk_used_delta": disk_delta, "disk_free": disk_free,
            "jobs": self.args.jobs, "paused": self.paused.is_set(),
            "total": len(rs), "counts": counts, "backends": backends,
            "building": building, "failures_recent": fails,
            "cohorts_ready": self.venvs.ready_count() if self.venvs else 0,
            "phase": phase, "phase_total": ptot, "phase_done": pdone,
            "phase_label": plabel, "phase_error": perr,
            "cohorts": [{"key": k, "count": v["count"],
                         "requirements": v["requirements"]} for k, v in cohorts],
            "op_stats": op_stats, "phase_durations": phase_dur,
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

        self._set(slug, status="building", started=time.time(), worker=wid,
                  ended=0.0, error="", note="", backend="", log=log_rel)
        self._emit("started", slug, worker=wid)
        try:
            mirror, err = timed("mirror", lambda: ensure_mirror(
                self.archive, fam.repo_url, fam.commit, self.args.mirror_missing))
            flog("mirror: " + (f"ok ({mirror.name})" if not err else f"FAIL {err}"))
            if err:
                return self._fail(slug, err)
            err = timed("extract", lambda: extract_tree(mirror, fam.commit, work, EXTRACT_TIMEOUT))
            flog("extract: " + ("ok" if not err else f"FAIL {err}"))
            if err:
                return self._fail(slug, err)

            if self.venvs is not None:
                req = read_requirements(work)

                def installing(key):
                    self._set(slug, note=f"installing deps ({key})")
                    self._emit("venv", slug, cohort=key)
                    flog(f"venv: installing cohort {key}…")
                python, cohort, verr = timed("venv", lambda: self.venvs.get_python(req, installing))
                self._set(slug, cohort=cohort, note="")
                flog(f"venv: cohort {cohort} " + ("ok" if not verr else f"FAIL {verr}"))
                if verr:
                    return self._fail(slug, f"venv: {verr}")
            else:
                python = self.args.build_python

            order = self._backend_order()
            ok, berr, used = False, "", ""
            for i, b in enumerate(order):
                if i > 0:
                    err = timed("extract", lambda: extract_tree(mirror, fam.commit, work, EXTRACT_TIMEOUT))
                    if err:
                        berr = err
                        break
                preclean_outputs(work)
                cfg, label, cerr = timed("config", lambda: resolve_config(self.google_fonts, fam, work))
                if cerr:
                    berr = cerr
                    flog(f"config: FAIL {cerr}")
                    break
                self._set(slug, backend=b, config_used=label)
                flog(f"build[{b}]: config={label} — running gftools.builder…")
                t0 = time.time()
                ok, berr = run_builder(python, cfg, work, log_path, self.args.timeout, b, self.args.fontc_bin)
                dt = time.time() - t0
                self._record_op(slug, "build", dt)
                flog(f"build[{b}]: " + ("OK" if ok else f"FAIL {berr}") + f"  ({dt:.0f}s)")
                if ok:
                    used = b
                    break
            if not ok:
                return self._fail(slug, berr or "build failed")

            nbytes, built = timed("collect", lambda: collect_outputs(work, out_dir, fam.shipped_fonts))
            if fam.shipped_fonts and not built:
                flog("collect: FAIL produced no expected font files")
                return self._fail(slug, f"{used}: produced no expected font files")
            missing = [f for f in fam.shipped_fonts if f not in built]
            cmp_label = ""
            if self.args.compare:
                cmp_label = timed("compare", lambda: compare_to_shipped(self.google_fonts, fam, built))
            flog(f"DONE: backend={used} bytes={nbytes} missing={len(missing)} compare={cmp_label or '-'}")
            self._set(slug, status="built", ended=time.time(), out_bytes=nbytes,
                      out_missing=len(missing), compare=cmp_label, backend=used, note="")
            self._emit("built", slug, backend=used, bytes=nbytes, compare=cmp_label,
                       missing=len(missing), dur=round(self.results[slug].dur(), 1))
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

    # ---- lifecycle: a background driver runs the phases (archive → cohorts → build)
    def run(self):
        self._status_thread = threading.Thread(target=self._status_writer, daemon=True)
        self._status_thread.start()
        self.driver = threading.Thread(target=self._drive, daemon=True)
        self.driver.start()

    def _drive(self):
        try:
            # Phase: populate the archive (mirror any missing upstream repos) — append-only.
            # `stop` is threaded through so Ctrl-C aborts these long phases promptly.
            if getattr(self.args, "populate_archive", False) and self.families and not self.stop.is_set():
                urls = sorted({f.repo_url for f in self.families.values()})
                self._begin_phase("archive", len(urls))
                populate_archive(urls, self.archive, self.args.jobs,
                                 on_progress=lambda d, t, u: self._phase_progress(d, u),
                                 stop=self.stop)
            # Phase: scan/generate the dependency cohorts (read-only)
            if self.families and not self.stop.is_set():
                self._begin_phase("cohorts", len(self.families))
                groups, sigs = scan_cohorts(
                    list(self.families.values()), self.archive, self.args.jobs,
                    on_progress=lambda d, t, s: self._phase_progress(d, s), stop=self.stop)
                with self.lock:
                    self.cohorts = {k: {"count": len(v), "requirements": sigs.get(k, "")}
                                    for k, v in sorted(groups.items(), key=lambda kv: -len(kv[1]))}
            # Phase: build
            if not self.stop.is_set():
                if self.venvs is not None:
                    self.venvs.ensure_base()
                self._begin_phase("build", self.q.qsize())
                for i in range(max(1, self.args.jobs)):
                    t = threading.Thread(target=self.worker, args=(i + 1,), daemon=True)
                    t.start()
                    self.workers.append(t)
                # exits only once every worker has stopped (no _emit can follow)
                while any(t.is_alive() for t in self.workers):
                    if self.stop.is_set() or self.all_done():
                        self.stop.set()
                    time.sleep(0.2)
        except Exception as e:
            with self.lock:
                self.phase_error = str(e)
        finally:
            self.save_state()
            self._set_phase("done")  # workers are guaranteed stopped here
            self._status_stop.set()                       # stop the periodic writer first…
            if self._status_thread is not None:
                self._status_thread.join(timeout=3)
            self.write_timings()
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
                          f"{snap['phase_total']}  disk +{human(snap['disk_used_delta'])}", flush=True)
                else:
                    print(f"  -- {hms(snap['elapsed'])}  built {c['built']} failed {c['failed']} "
                          f"building {c['building']} queued {c['queued']}  "
                          f"disk +{human(snap['disk_used_delta'])} "
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
            print(f"curses unavailable ({e}); using --ui plain.", file=sys.stderr)
            return PlainFrontend(self.orch).run()
        try:
            curses.wrapper(self._draw)
        except Exception as e:
            print(f"curses error ({e}); switching to plain output.", file=sys.stderr)
            return PlainFrontend(self.orch).run()

    PHASE_LABEL = {"init": "starting…", "archive": "populating archive (mirroring repos)",
                   "cohorts": "scanning dependency cohorts", "build": "building", "done": "done"}

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
        view, scroll = "overview", 0
        while True:
            if self.orch.stop.is_set():
                break
            ch = stdscr.getch()
            if ch in (ord("q"), ord("Q")):
                self.orch.stop.set(); break
            elif ch in (ord("p"), ord("P")):
                (self.orch.paused.clear if self.orch.paused.is_set() else self.orch.paused.set)()
            elif ch in (ord("1"), ord("o"), ord("O")):
                view, scroll = "overview", 0
            elif ch in (ord("2"), ord("c"), ord("C")):
                view, scroll = "cohorts", 0
            elif ch in (ord("3"), ord("f"), ord("F")):
                view, scroll = "failures", 0
            elif ch in (ord("4"), ord("s"), ord("S")):
                view, scroll = "stats", 0
            elif ch == 9:  # Tab cycles views
                view = {"overview": "cohorts", "cohorts": "failures",
                        "failures": "stats", "stats": "overview"}[view]
                scroll = 0
            elif ch == curses.KEY_DOWN:
                scroll += 1
            elif ch == curses.KEY_UP:
                scroll = max(0, scroll - 1)

            snap = self.orch.snapshot()
            c, bk = snap["counts"], snap["backends"]
            h, w = stdscr.getmaxyx()
            stdscr.erase()

            def put(y, x, s, attr=0):
                if 0 <= y < h and 0 <= x < w:
                    stdscr.addnstr(y, x, str(s), max(0, w - x - 1), attr)

            grand = snap["total"] or 1
            done = c["built"] + c["failed"]
            ph = snap["phase"]
            plabel = self.PHASE_LABEL.get(ph, ph)
            # header
            put(0, 0, " Google Fonts library build" + (" [PAUSED]" if snap["paused"] else ""),
                curses.A_BOLD)
            put(0, max(0, w - 24), f"elapsed {hms(snap['elapsed'])}", curses.A_BOLD)
            put(1, 0, f" disk +{human(snap['disk_used_delta'])}  free {human(snap['disk_free'])}  "
                      f"jobs {snap['jobs']}  cohorts {len(snap['cohorts'])}  "
                      f"fontc {bk['fontc']}/fontmake {bk['fontmake']}", CYAN)
            # phase banner + progress
            phasey = ph in ("archive", "cohorts") and snap["phase_total"]
            if phasey:
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
            # tabs
            x = 1
            for lbl, name in (("1 overview", "overview"), ("2 cohorts", "cohorts"),
                              ("3 failures", "failures"), ("4 stats", "stats")):
                put(4, x, f" {lbl} ", curses.A_REVERSE if view == name else curses.A_DIM)
                x += len(lbl) + 3
            put(4, max(x + 2, w - 24), "[tab]switch [↑↓]scroll", curses.A_DIM)

            row = 6
            if view == "overview":
                put(row, 0, " Now building ".ljust(w - 1, "-"), curses.A_BOLD); row += 1
                cap = max(0, (h - row) // 2 - 1)
                for bld in snap["building"][:cap]:
                    tag = bld["note"] or bld["backend"] or ""
                    put(row, 1, f"w{bld['worker']:>2} {bld['slug']:<36} {hms(bld['dur']):>8}  {tag}", YEL)
                    row += 1
                if not snap["building"]:
                    put(row, 1, "(idle)" if ph in ("build", "done") else f"… {plabel}"); row += 1
                row += 1
                put(row, 0, f" Recent failures ({c['failed']}) ".ljust(w - 1, "-"), curses.A_BOLD); row += 1
                for f in snap["failures_recent"][:max(0, h - row - 2)]:
                    put(row, 1, f"{f['slug']:<36} {f['error']}", RED); row += 1
            elif view == "cohorts":
                cohorts = snap["cohorts"]
                put(row, 0, f" Dependency cohorts ({len(cohorts)}) — live ".ljust(w - 1, "-"),
                    curses.A_BOLD); row += 1
                vis = max(1, h - row - 1)
                scroll = min(scroll, max(0, len(cohorts) - vis))
                for co in cohorts[scroll:scroll + vis]:
                    reqs = co["requirements"].splitlines()
                    sig = (reqs[0][:48] if reqs else "(no requirements)")
                    put(row, 1, f"{co['count']:>4}  {co['key']:<16} {sig}",
                        CYAN if co["key"] != "base" else 0); row += 1
                if not cohorts:
                    put(row, 1, f"(cohorts appear during the '{self.PHASE_LABEL['cohorts']}' phase)")
            elif view == "failures":
                fails = snap["failures_recent"]
                put(row, 0, f" Failures ({c['failed']}) — newest first ".ljust(w - 1, "-"),
                    curses.A_BOLD); row += 1
                vis = max(1, h - row - 1)
                scroll = min(scroll, max(0, len(fails) - vis))
                for f in fails[scroll:scroll + vis]:
                    put(row, 1, f"{f['slug']:<34} {f['error']}", RED); row += 1
                if not fails:
                    put(row, 1, "(no failures)", GREEN)
            elif view == "stats":
                put(row, 0, " Timing — phases ".ljust(w - 1, "-"), curses.A_BOLD); row += 1
                for ph, sec in sorted(snap.get("phase_durations", {}).items(),
                                      key=lambda kv: -kv[1]):
                    put(row, 1, f"{ph:<12} {hms(sec)}"); row += 1
                row += 1
                put(row, 0, " Timing — operations (total / count / mean / max, s) ".ljust(w - 1, "-"),
                    curses.A_BOLD); row += 1
                ops = sorted(snap.get("op_stats", {}).items(), key=lambda kv: -kv[1]["total"])
                vis = max(1, h - row - 1)
                scroll = min(scroll, max(0, len(ops) - vis))
                for op, s in ops[scroll:scroll + vis]:
                    put(row, 1, f"{op:<10} total {s['total']:>9.1f}   n {s['count']:>5}   "
                                f"mean {s['mean']:>7.2f}   max {s['max']:>7.1f}", CYAN); row += 1
                if not ops:
                    put(row, 1, "(timing accrues as builds run)")

            mon = getattr(self, "monitor", False)
            foot = (" [q]uit monitor (build keeps running)" if mon else " [q]uit [p]ause")
            if mon and not snap.get("daemon_alive", True):
                foot = " [q]uit   ⚠ daemon not running (build finished or stopped)"
            put(h - 1, 0, foot + "   logs: " + str(self.orch.build_dir / "logs"), curses.A_DIM)
            stdscr.refresh()
            if snap["done"] and not self.monitor:
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
    """Attach a read-only live monitor to a (possibly detached) build at build_dir."""
    fe = FRONTENDS[ui if ui in FRONTENDS else "plain"](MonitorState(build_dir))
    fe.monitor = True
    fe.run()


def print_timing_summary(data: dict):
    e = sys.stderr
    print("\nTiming (per-phase + per-operation; full detail in timings.json):", file=e)
    for ph, sec in sorted(data.get("phases", {}).items(), key=lambda kv: -kv[1]):
        print(f"  phase {ph:<10} {hms(sec)}", file=e)
    for op, s in sorted(data.get("operations", {}).items(), key=lambda kv: -kv[1]["total"]):
        print(f"  op    {op:<10} total {s['total']:>9.1f}s  n {s['count']:>5}  "
              f"mean {s['mean']:>6.2f}s  max {s['max']:>6.1f}s", file=e)


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
               "base_requirements", "compare", "data_dir")


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
              "cohorts": [], "total": 0, "elapsed": 0, "disk_used_delta": 0, "disk_free": 0,
              "jobs": 0, "paused": False, "phase_total": 0, "phase_done": 0, "phase_label": "",
              "phase_error": "", "op_stats": {}, "phase_durations": {}, "done": False}

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


def setup_wizard(spec, plan_fn):
    """Interactive ncurses settings form. `spec` is a list of field dicts:
       {key, label, type: text|path|int|stepnum|bool|choice, value, choices?, step?, min?,
        max?, show_if?(values)->bool}.
    Fields are pre-populated with the resolved defaults; the user edits them (editable
    fields have a movable text cursor; `stepnum` also reacts to ←/→ with ±step; `choice`
    cycles with ←/→/space; `bool` toggles with space; conditional fields appear/disappear via
    `show_if`) while a live 'Plan' (plan_fn(values)->[str]) updates. Returns the edited
    {key: typed value} dict to proceed, or None to cancel. Raises if curses is unusable."""
    import curses
    EDIT = ("text", "path", "int", "stepnum")
    fields = []
    for f in spec:
        f = dict(f)
        if f["type"] in EDIT:
            f["value"] = str(f["value"])
            f["_caret"] = len(f["value"])
        fields.append(f)
    by_key = {f["key"]: f for f in fields}
    buttons = ["Start", "Cancel"]
    VALCOL = 38

    def typed():
        out = {}
        for f in fields:
            t, v = f["type"], f["value"]
            if t == "int":
                out[f["key"]] = int(v) if str(v).strip().lstrip("-").isdigit() else 0
            elif t == "stepnum":
                try:
                    out[f["key"]] = float(v)
                except (TypeError, ValueError):
                    out[f["key"]] = 0.0
            elif t == "bool":
                out[f["key"]] = bool(v)
            else:
                out[f["key"]] = v
        return out

    def visible(vals):
        return [f for f in fields if "show_if" not in f or f["show_if"](vals)]

    def form(stdscr):
        stdscr.keypad(True)
        active = fields[0]["key"]
        while True:
            vals = typed()
            vis = visible(vals)
            nav = [f["key"] for f in vis] + buttons
            if active not in nav:
                active = nav[0]
            stdscr.erase()
            h, w = stdscr.getmaxyx()
            cursor = None

            def put(y, x, s, a=0):
                if 0 <= y < h and 0 <= x < w:
                    stdscr.addnstr(y, x, str(s), max(0, w - x - 1), a)

            put(0, 0, " gflib-build — setup wizard", curses.A_BOLD)
            put(1, 0, " [↑↓/Tab] move   [space] toggle   [←→] move cursor / step / cycle   "
                      "type to edit   [Esc] cancel", curses.A_DIM)
            row = 3
            for f in vis:
                act = active == f["key"]
                if f["type"] == "bool":
                    val = "[x] yes" if f["value"] else "[ ] no"
                    put(row, VALCOL, val, curses.A_REVERSE if act else 0)
                elif f["type"] == "choice":
                    put(row, VALCOL, f"‹ {f['value']} ›", curses.A_REVERSE if act else 0)
                else:
                    put(row, VALCOL, f["value"] if f["value"] else "")
                    if act:
                        cursor = (row, VALCOL + min(f.get("_caret", len(f["value"])), len(f["value"])))
                put(row, 1, ("▸ " if act else "  ") + f["label"], curses.A_BOLD if act else 0)
                row += 1
            row += 1
            put(row, 0, " Plan ".ljust(max(1, w - 1), "-"), curses.A_BOLD); row += 1
            for line in plan_fn(vals)[:max(0, h - row - 3)]:
                put(row, 2, line); row += 1
            x = 2
            for b in buttons:
                put(h - 2, x, f" {b} ", curses.A_REVERSE if active == b else curses.A_BOLD)
                x += len(b) + 4
            try:
                if cursor:
                    curses.curs_set(1)
                    stdscr.move(min(cursor[0], h - 1), min(cursor[1], w - 1))
                else:
                    curses.curs_set(0)
            except curses.error:
                pass
            stdscr.refresh()

            ch = stdscr.getch()
            ai = nav.index(active)
            if ch == 27:
                return None
            elif ch == curses.KEY_UP:
                active = nav[(ai - 1) % len(nav)]
            elif ch in (curses.KEY_DOWN, 9):
                active = nav[(ai + 1) % len(nav)]
            elif active in buttons:
                if ch in (10, 13, ord(" ")):
                    return typed() if active == "Start" else None
            else:
                f = by_key[active]
                t = f["type"]
                if t == "bool":
                    if ch in (ord(" "), 10, 13):
                        f["value"] = not f["value"]
                elif t == "choice":
                    ci = f["choices"].index(f["value"])
                    if ch in (ord(" "), curses.KEY_RIGHT):
                        f["value"] = f["choices"][(ci + 1) % len(f["choices"])]
                    elif ch == curses.KEY_LEFT:
                        f["value"] = f["choices"][(ci - 1) % len(f["choices"])]
                    elif ch in (10, 13):
                        active = nav[(ai + 1) % len(nav)]
                else:                                     # editable text / int / stepnum
                    cur = f.get("_caret", len(f["value"]))
                    if t == "stepnum" and ch in (curses.KEY_LEFT, curses.KEY_RIGHT):
                        step = f.get("step", 5) * (1 if ch == curses.KEY_RIGHT else -1)
                        try:
                            x = float(f["value"] or 0)
                        except ValueError:
                            x = 0.0
                        x = max(f.get("min", 0), min(f.get("max", 100), x + step))
                        f["value"] = f"{x:g}"; f["_caret"] = len(f["value"])
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
                            f["value"] = f["value"][:cur - 1] + f["value"][cur:]; f["_caret"] = cur - 1
                    elif ch == curses.KEY_DC:
                        if cur < len(f["value"]):
                            f["value"] = f["value"][:cur] + f["value"][cur + 1:]
                    elif ch in (10, 13):
                        active = nav[(ai + 1) % len(nav)]
                    elif 32 <= ch < 127:
                        c = chr(ch)
                        ok = (t == "text" or t == "path"
                              or (t == "int" and c.isdigit())
                              or (t == "stepnum" and (c.isdigit() or c == ".")))
                        if ok:
                            f["value"] = f["value"][:cur] + c + f["value"][cur:]; f["_caret"] = cur + 1

    return curses.wrapper(form)


def setup_wizard_plain(steps, gf_path, archive, build_dir, args) -> bool:
    """Fallback plain-terminal confirmation (used if curses is unavailable)."""
    e = sys.stderr
    print("\n=== gflib-build setup ===", file=e)
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
                    help="skip the setup wizard (non-interactive bootstrap with current settings)")
    ap.add_argument("--wizard", action="store_true",
                    help="always show the interactive setup wizard (even when nothing needs bootstrapping)")
    ap.add_argument("--backend", choices=["auto", "fontc", "fontmake"], default="auto")
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
    ap.set_defaults(manage_venvs=False, compare=False)   # pin defaults (overridden by config/CLI)
    return ap


def _plan_lines(gf, archive, src, populate, percent, backend, jobs, manage_venvs, compare):
    lines = []
    if src == "metadata":
        if gf and (gf / "ofl").is_dir():
            lines.append(f"google/fonts : use existing clone ({gf})")
        else:
            lines.append(f"google/fonts : CLONE {GOOGLE_FONTS_URL} → {gf or '(unset!)'}")
    else:
        lines.append("worklist     : every mirror in the archive (google/fonts optional)")
    if populate:
        lines.append(f"archive      : POPULATE — mirror any missing upstream repos → {archive}")
    else:
        lines.append(f"archive      : use as-is ({archive})" + ("" if archive.is_dir() else "  [ABSENT]"))
    scope = "full library" if percent >= 100 else f"{percent:g}% sample"
    extra = ("  +venvs" if manage_venvs else "") + ("  +compare" if compare and src == "metadata" else "")
    lines.append(f"build        : backend={backend}  jobs={jobs}  scope={scope}{extra}")
    return lines


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
    if sys.platform == "win32":
        sys.exit("gflib-build targets macOS/Linux (POSIX venv layout, git archive, tar).")

    # ---- attach / stop an existing (possibly detached) build, then exit ----
    mon_build_dir = Path(args.build_dir) if args.build_dir else (Path(args.data_dir) / "build")
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
        run_monitor(mon_build_dir, pick_frontend(args.ui))
        return

    # ---- resolve paths (defaults from --data-dir; auto-detect where possible) ----
    data_dir = Path(args.data_dir)
    gf = (Path(args.google_fonts) if args.google_fonts
          else (data_dir / "google-fonts" if args.source == "metadata" else None))
    if args.archive:
        archive = Path(args.archive)
    else:
        det = detect_archive(data_dir)        # auto-detect a pre-existing archive
        archive = Path(det) if det else (data_dir / "archive")
    build_dir = Path(args.build_dir) if args.build_dir else (data_dir / "build")
    if not args.fontc_bin:
        args.fontc_bin = detect_fontc()       # auto-detect a fontc binary
    read_only = args.list or args.cohorts_report
    if args.populate_archive is None:
        args.populate_archive = (args.source == "metadata") and not read_only

    need_gf_clone = args.source == "metadata" and not (gf and (gf / "ofl").is_dir())
    want_build_fontc = False
    steps = []
    if need_gf_clone:
        steps.append(("clone google/fonts", f"{GOOGLE_FONTS_URL} → {gf} (shallow)"))
    if args.populate_archive:
        steps.append(("populate archive", f"mirror missing upstream repos into {archive}"))

    # ---- interactive ncurses setup wizard (editable, pre-populated fields) ----
    if (steps or args.wizard) and not args.yes and not read_only:
        if not sys.stdin.isatty():
            sys.exit("missing prerequisites (google/fonts clone and/or archive). Re-run with "
                     "--yes to bootstrap non-interactively, or pass --google-fonts/--archive.")
        spec = [
            {"key": "source", "label": "worklist source", "type": "choice",
             "value": args.source, "choices": ["metadata", "archive"]},
            {"key": "google_fonts", "label": "google/fonts clone", "type": "path",
             "value": str(gf) if gf else "", "show_if": lambda v: v["source"] == "metadata"},
            {"key": "archive", "label": "repo archive", "type": "path", "value": str(archive)},
            {"key": "build_dir", "label": "build output dir", "type": "path", "value": str(build_dir)},
            {"key": "backend", "label": "build backend", "type": "choice",
             "value": args.backend, "choices": ["auto", "fontc", "fontmake"]},
            {"key": "fontc_bin", "label": "fontc binary (auto-detected)", "type": "path",
             "value": args.fontc_bin or "", "show_if": lambda v: v["backend"] != "fontmake"},
            {"key": "build_fontc", "label": "build fontc from source (if none)", "type": "bool",
             "value": False,
             "show_if": lambda v: v["backend"] != "fontmake" and not v["fontc_bin"]},
            {"key": "jobs", "label": "parallel jobs", "type": "int", "value": str(args.jobs)},
            {"key": "percent", "label": "percent of library (←/→ ±5)", "type": "stepnum",
             "value": f"{args.percent:g}", "step": 5, "min": 1, "max": 100},
            {"key": "use_timeout", "label": "use per-build timeout", "type": "bool",
             "value": args.timeout is not None},
            {"key": "timeout_seconds", "label": "  timeout seconds", "type": "int",
             "value": str(args.timeout if args.timeout is not None else 1800),
             "show_if": lambda v: v["use_timeout"]},
            {"key": "populate_archive", "label": "populate archive (mirror missing)", "type": "bool",
             "value": bool(args.populate_archive)},
            {"key": "manage_venvs", "label": "cohort venvs (--manage-venvs)", "type": "bool",
             "value": bool(args.manage_venvs)},
            {"key": "compare", "label": "compare to shipped (metadata only)", "type": "bool",
             "value": bool(args.compare), "show_if": lambda v: v["source"] == "metadata"},
        ]

        def plan_fn(v):
            g = Path(v["google_fonts"]) if v.get("google_fonts") else None
            lines = _plan_lines(g, Path(v["archive"]), v["source"], v["populate_archive"],
                                v["percent"], v["backend"], v["jobs"], v["manage_venvs"],
                                v.get("compare", False))
            if v["backend"] != "fontmake":
                if v.get("fontc_bin"):
                    lines.append(f"fontc        : {v['fontc_bin']}")
                elif v.get("build_fontc"):
                    lines.append("fontc        : BUILD from source (cargo build --release)")
                    if not detect_cargo():
                        lines.append("  ⚠ cargo not found — " + RUST_INSTALL_HINT)
                else:
                    lines.append("fontc        : none — 'auto' falls back to fontmake")
            lines.append("timeout      : " + (f"{v['timeout_seconds']}s"
                                              if v.get("use_timeout") else "none (stop manually)"))
            return lines
        try:
            edited = setup_wizard(spec, plan_fn)
        except Exception:                               # curses unusable → plain confirm
            edited = {} if setup_wizard_plain(steps, gf, archive, build_dir, args) else None
        if edited is None:
            sys.exit("aborted.")
        if edited:                                      # apply the user's edits
            args.source = edited["source"]
            gf = Path(edited["google_fonts"]) if edited.get("google_fonts") else None
            archive = Path(edited["archive"])
            build_dir = Path(edited["build_dir"])
            args.backend = edited["backend"]
            args.fontc_bin = edited.get("fontc_bin") or None
            want_build_fontc = bool(edited.get("build_fontc")) and args.backend != "fontmake"
            args.jobs = max(1, edited["jobs"])
            args.percent = edited["percent"]
            args.timeout = int(edited["timeout_seconds"]) if edited.get("use_timeout") else None
            args.populate_archive = edited["populate_archive"]
            args.manage_venvs = edited["manage_venvs"]
            args.compare = edited.get("compare", False) and args.source == "metadata"
            need_gf_clone = args.source == "metadata" and not (gf and (gf / "ofl").is_dir())

    # ---- auto-default base requirements to the bundled file (so cohort venvs just work) ----
    if args.manage_venvs and not args.base_requirements:
        bundled = Path(__file__).resolve().parent / "requirements-build.txt"
        if bundled.is_file():
            args.base_requirements = str(bundled)

    # ---- finalize + validate FIRST — before any expensive clone/build (fail fast) ----
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

    # ---- now the expensive bootstrap, only after all validations have passed ----
    if need_gf_clone:
        try:
            ensure_google_fonts(gf, on_progress=lambda m: print(f"  {m}", file=sys.stderr))
        except (RuntimeError, ValueError) as e:
            sys.exit(str(e))
    if want_build_fontc and not args.fontc_bin:
        try:
            args.fontc_bin = build_fontc_from_source(
                data_dir / "fontc", on_progress=lambda m: print(f"  {m}", file=sys.stderr))
        except RuntimeError as e:
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

    if args.cohorts_report:
        sel = sampled
        if args.only:
            keep = set(args.only.split(","))
            sel = [f for f in sampled if f.slug in keep]
        cohorts_report(sel, Path(args.archive), args.jobs, Path(args.build_dir) / "cohorts.json",
                       context=ctx)
        return

    orch = Orchestrator(args, sampled, total, skipped)
    print(f"Selected {len(sampled)}/{len(families)} ({args.percent:g}%) via --source "
          f"{args.source}{ctx}. Queued {orch.q.qsize()}; backend={args.backend}"
          f"{' (fontc-first)' if orch._backend_order()[0] == 'fontc' else ''}.", file=sys.stderr)

    # register handlers BEFORE daemonize so the daemon inherits them (no kill-before-handler window)
    signal.signal(signal.SIGINT, lambda *_: orch.stop.set())
    signal.signal(signal.SIGTERM, lambda *_: orch.stop.set())

    if args.detach:
        if daemonize(build_dir):                  # detached daemon: run the build, headless
            orch.run()
            orch.join()
            orch.save_state()
            return
        # original parent: drop our copy of the daemon's events file, then attach a monitor
        orch._close_events()
        print(f"build running detached. reattach any time:\n"
              f"  python3 {Path(sys.argv[0]).name} --attach --build-dir {build_dir}\n"
              f"  python3 {Path(sys.argv[0]).name} --stop   --build-dir {build_dir}",
              file=sys.stderr)
        time.sleep(0.5)
        if pick_frontend(args.ui) != "none":
            run_monitor(build_dir, pick_frontend(args.ui))
        return

    orch.run()
    frontend = FRONTENDS[pick_frontend(args.ui)](orch)
    try:
        frontend.run()
        orch.join()
    finally:
        orch.save_state()

    c = orch.snapshot()["counts"]
    print(f"\nDONE: built {c['built']}, failed {c['failed']}. "
          f"state: {orch.state_path}  events: {orch.build_dir / 'events.jsonl'}", file=sys.stderr)
    print_timing_summary(orch.write_timings())


if __name__ == "__main__":
    main()
