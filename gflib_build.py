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
    fams, total, skipped = [], 0, 0
    for lic in LICENSE_DIRS:
        base = google_fonts / lic
        if not base.is_dir():
            continue
        for meta in sorted(base.glob("*/METADATA.pb")):
            parsed = parse_metadata(meta)
            if parsed is None:
                continue
            total += 1
            name, repo, commit, cfg, fonts = parsed
            slug = f"{lic}/{meta.parent.name}"
            has_override = (google_fonts / slug / "config.yaml").is_file()
            if not commit or not (has_override or cfg):
                skipped += 1
                continue
            fams.append(Family(slug, name, repo, commit, cfg, has_override, fonts))
    return fams, total, skipped


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


# ============================================================================== config

def resolve_config(google_fonts: Path, fam: Family, work: Path):
    override = google_fonts / fam.slug / "config.yaml"
    if override.is_file():
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
        norm = normalize_requirements(req_text)
        if not norm:
            return "base"
        return "c-" + hashlib.sha1(norm.encode()).hexdigest()[:12]

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
                timeout: int, backend: str, fontc_bin: Optional[str]):
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
    with open(log_path, "wb") as logf:
        logf.write(f"# backend={backend}\n# {' '.join(cmd)}\n# cwd={work}\n\n".encode())
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
        self.google_fonts = Path(args.google_fonts)
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

        self.venvs: Optional[VenvManager] = None
        if args.manage_venvs:
            self.venvs = VenvManager(self.build_dir, args.base_python,
                                     Path(args.base_requirements) if args.base_requirements else None)

        self._load_state()
        self._enqueue()

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
        return {
            "elapsed": time.time() - self.start_time,
            "disk_used_delta": disk_delta, "disk_free": disk_free,
            "jobs": self.args.jobs, "paused": self.paused.is_set(),
            "total": len(rs), "counts": counts, "backends": backends,
            "building": building, "failures_recent": fails,
            "cohorts_ready": self.venvs.ready_count() if self.venvs else 0,
            "done": self.all_done(),
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
        self._set(slug, status="building", started=time.time(), worker=wid,
                  ended=0.0, error="", note="", backend="")
        self._emit("started", slug, worker=wid)
        try:
            mirror, err = ensure_mirror(self.archive, fam.repo_url, fam.commit,
                                        self.args.mirror_missing)
            if err:
                return self._fail(slug, err)
            err = extract_tree(mirror, fam.commit, work, self.args.timeout)
            if err:
                return self._fail(slug, err)

            # pick interpreter (cohort venv or single build-python) from the extracted tree
            if self.venvs is not None:
                req = read_requirements(work)

                def installing(key):
                    self._set(slug, note=f"installing deps ({key})")
                    self._emit("venv", slug, cohort=key)
                python, cohort, verr = self.venvs.get_python(req, installing)
                self._set(slug, cohort=cohort, note="")
                if verr:
                    return self._fail(slug, f"venv: {verr}")
            else:
                python = self.args.build_python

            # backend attempts; each fallback gets a FRESH extraction (truly from scratch)
            order = self._backend_order()
            ok, berr, used = False, "", ""
            for i, b in enumerate(order):
                if i > 0:
                    err = extract_tree(mirror, fam.commit, work, self.args.timeout)
                    if err:
                        berr = err
                        break
                preclean_outputs(work)
                cfg, label, cerr = resolve_config(self.google_fonts, fam, work)
                if cerr:
                    berr = cerr
                    break
                log_rel = f"logs/{safe}.{b}.log"
                self._set(slug, backend=b, config_used=label, log=log_rel)
                ok, berr = run_builder(python, cfg, work, self.build_dir / log_rel,
                                       self.args.timeout, b, self.args.fontc_bin)
                if ok:
                    used = b
                    break
            if not ok:
                return self._fail(slug, berr or "build failed")

            nbytes, built = collect_outputs(work, out_dir, fam.shipped_fonts)
            if fam.shipped_fonts and not built:
                return self._fail(slug, f"{used}: produced no expected font files")
            missing = [f for f in fam.shipped_fonts if f not in built]
            cmp_label = (compare_to_shipped(self.google_fonts, fam, built)
                         if self.args.compare else "")
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

    # ---- lifecycle
    def run(self):
        if self.venvs is not None:
            self.venvs.ensure_base()
        for i in range(max(1, self.args.jobs)):
            t = threading.Thread(target=self.worker, args=(i + 1,), daemon=True)
            t.start()
            self.workers.append(t)

    def join(self):
        while any(t.is_alive() for t in self.workers):
            if self.all_done():
                self.stop.set()
            time.sleep(0.2)
        self.save_state()
        try:
            self._events.close()
        except Exception:
            pass


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
    def __init__(self, orch: "Orchestrator"):
        self.orch = orch
    def run(self):
        raise NotImplementedError


class NoneFrontend(Frontend):
    def run(self):
        while any(t.is_alive() for t in self.orch.workers):
            if self.orch.all_done():
                self.orch.stop.set()
                break
            time.sleep(0.3)


class PlainFrontend(Frontend):
    """Traditional terminal output: one line per completion + periodic summaries."""
    def run(self):
        seen, last = set(), 0.0
        while True:
            snap = self.orch.snapshot()
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
                print(f"  -- {hms(snap['elapsed'])}  built {c['built']} failed {c['failed']} "
                      f"building {c['building']} queued {c['queued']}  disk +{human(snap['disk_used_delta'])} "
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

    def _draw(self, stdscr):
        import curses
        stdscr.nodelay(True)
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
        while True:
            if self.orch.stop.is_set():
                break
            ch = stdscr.getch()
            if ch in (ord("q"), ord("Q")):
                self.orch.stop.set()
                break
            if ch in (ord("p"), ord("P")):
                (self.orch.paused.clear if self.orch.paused.is_set() else self.orch.paused.set)()
            snap = self.orch.snapshot()
            c, bk = snap["counts"], snap["backends"]
            h, w = stdscr.getmaxyx()
            stdscr.erase()

            def put(y, x, s, attr=0):
                if 0 <= y < h and 0 <= x < w:
                    stdscr.addnstr(y, x, s, max(0, w - x - 1), attr)

            done = c["built"] + c["failed"]
            grand = snap["total"] or 1
            put(0, 0, " Google Fonts library build" + (" [PAUSED]" if snap["paused"] else ""),
                curses.A_BOLD)
            put(0, max(0, w - 24), f"elapsed {hms(snap['elapsed'])}", curses.A_BOLD)
            put(1, 0, f" disk: +{human(snap['disk_used_delta'])} used   free {human(snap['disk_free'])}"
                      f"   jobs {snap['jobs']}   cohorts {snap['cohorts_ready']}", CYAN)
            put(3, 0, " Built", curses.A_BOLD); put(3, 7, f"{c['built']}/{grand}", GREEN | curses.A_BOLD)
            put(3, 22, "Failed", curses.A_BOLD); put(3, 29, str(c["failed"]), RED | curses.A_BOLD)
            put(3, 38, "Building", curses.A_BOLD); put(3, 47, str(c["building"]), YEL | curses.A_BOLD)
            put(3, 54, "Queued", curses.A_BOLD); put(3, 61, str(c["queued"]))
            if w > 84:
                put(3, 70, f"fontc {bk['fontc']}/fontmake {bk['fontmake']}", CYAN)
            barw = max(10, w - 4)
            filled = int(barw * done / grand)
            put(4, 1, "[" + "#" * filled + "-" * (barw - filled) + "]")
            put(4, max(2, barw // 2), f" {100 * done // grand}% ", curses.A_BOLD)

            row = 6
            put(row, 0, " Now building ".ljust(w - 1, "-"), curses.A_BOLD); row += 1
            cap = max(0, (h - row) // 2 - 2)
            for bld in snap["building"][:cap]:
                tag = bld["note"] or (bld["backend"] or "")
                put(row, 1, f"w{bld['worker']:>2} {bld['slug']:<36} {hms(bld['dur']):>8}  {tag}", YEL)
                row += 1
            if not snap["building"]:
                put(row, 1, "(idle)"); row += 1
            row += 1
            put(row, 0, f" Recent failures ({c['failed']}) ".ljust(w - 1, "-"), curses.A_BOLD); row += 1
            for f in snap["failures_recent"][:max(0, h - row - 2)]:
                put(row, 1, f"{f['slug']:<36} {f['error']}", RED); row += 1
            put(h - 1, 0, " [q]uit  [p]ause/resume   logs: " + str(self.orch.build_dir / "logs"),
                curses.A_DIM)
            stdscr.refresh()
            if snap["done"]:
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


# ================================================================================= main

def build_argparser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        description="From-scratch, archive-safe, Rust-first full-library build of Google Fonts.")
    ap.add_argument("--google-fonts", required=True, help="path to a google/fonts clone")
    ap.add_argument("--archive", required=True, help="repo archive of bare mirrors ({owner}/{repo}.git)")
    ap.add_argument("--build-dir", required=True, help="output dir (NOT inside any repo)")
    ap.add_argument("--backend", choices=["auto", "fontc", "fontmake"], default="auto")
    ap.add_argument("--fontc-bin", default=None, help="path to the fontc (Rust) binary")
    ap.add_argument("--build-python", default=sys.executable,
                    help="interpreter for builds when --manage-venvs is off")
    ap.add_argument("--manage-venvs", action="store_true",
                    help="create & share one venv per dependency cohort")
    ap.add_argument("--base-python", default=sys.executable, help="python used to create cohort venvs")
    ap.add_argument("--base-requirements", default=None, help="pinned base toolchain requirements file")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--timeout", type=int, default=1800, help="per-family build timeout (s)")
    ap.add_argument("--percent", type=float, default=100.0,
                    help="build only this %% of the library (evenly-spaced sample) for validation")
    ap.add_argument("--only", default="", help="comma-separated slugs (e.g. ofl/dmsans)")
    ap.add_argument("--compare", action="store_true", help="sha256-compare built fonts to shipped")
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
    return ap


def main():
    args = build_argparser().parse_args()
    if sys.platform == "win32":
        sys.exit("gflib-build targets macOS/Linux (POSIX venv layout, git archive, tar).")
    gf = Path(args.google_fonts)
    if not (gf / "ofl").is_dir():
        sys.exit(f"--google-fonts {gf} has no ofl/ — is this a google/fonts clone?")
    if not Path(args.archive).is_dir():
        sys.exit(f"--archive {args.archive} not found")
    if args.backend == "fontc" and not args.fontc_bin:
        sys.exit("--backend fontc requires --fontc-bin")
    if args.manage_venvs and not args.base_requirements:
        sys.exit("--manage-venvs requires --base-requirements (the pinned base toolchain)")
    if not (0 < args.percent <= 100):
        sys.exit("--percent must be in (0, 100]")
    Path(args.build_dir).mkdir(parents=True, exist_ok=True)
    for sub in ("work", "out", "logs"):
        (Path(args.build_dir) / sub).mkdir(exist_ok=True)

    families, total, skipped = discover(gf)
    sampled = sample_evenly(families, args.percent)
    if args.list:
        for f in sampled:
            cfg = "override" if f.has_override else (f.config_yaml or "?")
            print(f"{f.slug:<40} {cfg:<26} {f.repo_url}")
        print(f"\n{len(sampled)} selected ({args.percent:g}% of {len(families)} buildable; "
              f"{total} with source, {skipped} skipped)", file=sys.stderr)
        return

    orch = Orchestrator(args, sampled, total, skipped)
    print(f"Selected {len(sampled)}/{len(families)} buildable ({args.percent:g}%). "
          f"Queued {orch.q.qsize()}; backend={args.backend}"
          f"{' (fontc-first)' if orch._backend_order()[0] == 'fontc' else ''}; "
          f"ui={pick_frontend(args.ui)}.", file=sys.stderr)

    signal.signal(signal.SIGINT, lambda *_: orch.stop.set())
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


if __name__ == "__main__":
    main()
