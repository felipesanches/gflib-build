# gflib-build

A from-scratch, **archive-safe** harness to build the **entire Google Fonts library**
on your own machine (outside the dev VM), with a **live ncurses dashboard**, a
**Rust-first** build strategy, and **dependency cohorts** that share virtual
environments.

> Status: **work in progress.** The core build pipeline, the live TUI, archive-safe
> pristine extraction, resumable state, dependency cohorts, and Rust-first/Python-
> fallback backend selection are implemented. See [Roadmap](#roadmap).

**Documentation:** this README is the overview + spec + usage.
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) covers the internals (pipeline, backends,
cohorts, concurrency, the `state.json`/`events.jsonl` schemas, archive-safety invariants);
[`docs/EXTENDING.md`](docs/EXTENDING.md) shows how to add a frontend (incl. a web UI) or a
backend; [`docs/cohort-map.md`](docs/cohort-map.md) is the generated full-library cohort map.

---

## Quick start — zero to built

With no existing data, a single command bootstraps everything. It runs a **setup wizard**
first (confirms before any heavy step), then drives the whole pipeline live in the TUI:

```sh
python3 gflib_build.py            # uses ./gflib-data/ for everything
```

The wizard is an **interactive ncurses form** with the settings pre-populated to their
defaults — you edit any field, watch the live "Plan" update, then choose **Start**:

```
 gflib-build — setup wizard
 [↑↓/Tab] move   [space] toggle   [←→] move cursor / step / cycle   type to edit   [Esc] cancel

 ▸ worklist source              ‹ metadata ›
   google/fonts clone           gflib-data/google-fonts
   repo archive                 /home/you/repo_archive        (auto-detected)
   build output dir             gflib-data/build
   build backend                ‹ auto ›
   fontc binary (auto-detected) /home/you/fontc/target/release/fontc
   parallel jobs                8
   percent of library (←/→ ±5)  100
   use per-build timeout        [ ] no
   populate archive (mirror missing)  [x] yes
   cohort venvs                 [ ] no
 Plan -----------------------------------------------------------------------
   google/fonts : use existing clone (…)
   archive      : POPULATE — mirror any missing upstream repos → /home/you/repo_archive
   build        : backend=auto  jobs=8  scope=full library
   fontc        : /home/you/fontc/target/release/fontc
   timeout      : none (stop manually)
  Start    Cancel
```

Every field is pre-filled with the resolved default and editable: `↑`/`↓` or `Tab` move
between fields; `space` toggles a checkbox; `←`/`→` move the **text cursor** (Home/End jump,
Backspace/Delete edit at the caret), **cycle** a `‹choice›`, or **±5-step** the percent;
type for finer/other values. Conditional fields appear as relevant — ticking **use
per-build timeout** reveals a seconds field (unticked = builds never time out, stop them in
the UI); the **fontc** binary is auto-detected, and if missing the form offers *build fontc
from source* or lets you type a path; the **repo archive** is auto-detected too. Navigate to
**Start** and press Enter (or `Esc`/Cancel). Then it runs the phases — **clone → populate
archive → generate cohorts → build** — live. `--yes` skips the wizard, `--wizard` forces it.

> Cloning google/fonts is a few GB; mirroring all upstream repos can be tens of GB and take
> a long time. Sources are read-only and **archives are never deleted**.

---

## Specifications (as given by Felipe)

This tool is built to the following requirements. They are recorded here verbatim in
intent so nothing is lost as the tool evolves.

1. **Run outside the VM.** Produce the build *rules* so they can be executed
   independently on the laptop (which now has plenty of local storage).
2. **Live, interactive terminal UI** (ncurses or a similar terminal-app library)
   giving real-time feedback on:
   - **(A)** which families were already built,
   - **(B)** which ones are being built at a given moment,
   - **(C)** how much space was used so far,
   - **(D)** how long the build has been running,
   - **(E)** how many failures happened so far — **with detailed logs of the failures**.
3. **Clean any pre-built font project** — to save space and to guarantee we build
   everything **from scratch**.
4. The build must be based on the **pristine original state of the cloned (archived)
   repos**.
5. The build procedure **must not change the repos themselves**; instead, save all
   assets in a **separate build directory**.
6. **Never delete the archives.**
7. **Save these specification messages in this README.** (This section.)
8. **Keep the build system in its own git repository, and commit often.**
9. **Optimize total build time** by employing **parallelism**.
10. **Be smart about installing Python dependencies** when they are needed for build
    steps. If many families use the same set of Python dependencies, place them in a
    **cohort** so they can **share a single virtual environment**.
11. **Reduce reliance on Python.** If a family can be built with **Rust +
    gftools-builder3 (fontc)**, that is the success path. Families that still need
    **gftools-builder2 / Python** (e.g. for pre-build steps or Python-only deps) are
    fine as their own cohorts.
12. **Long-term goal: migrate everything to Rust.** This tool also *measures* how much
    of the library already builds with the Rust path, to track that migration.
13. **The terminal UI must be optional and modular.** Some users prefer a traditional
    terminal program; others may feed everything to a web interface. Build it modular so
    others can customize the frontend.
14. **Allow building only a percentage of the library** (e.g. 5%) instead of the whole
    thing — useful for validating the tool during development.
15. **Bootstrap from nothing.** With `--source metadata` (and by default, no flags), the
    tool should `git clone` google/fonts, read all `METADATA.pb`, **create/populate a local
    archive** of upstream repos (building one from scratch if none exists), generate the
    cohorts, and build them all — so any user can bootstrap the whole process. All of it
    shown live on an ncurses UI the user can observe and **navigate** as the data updates.
16. **Setup-wizard mode.** It must **ask the user before** doing those heavy things
    (cloning, mirroring) — a setup wizard, not silent.
17. **The wizard is an editable ncurses form** with fields pre-populated with the default
    settings that the user can edit and then move on. Specifically: text fields render a
    movable cursor; a *build timeout* checkbox reveals a seconds field only when ticked
    (otherwise builds never time out — the user can still stop them in the UI); *percent of
    library* reacts to ←/→ in ±5 steps yet stays typeable for finer control; the *fontc*
    binary is auto-detected (offering to build it from source, or to type a path, if not
    found); and a pre-existing **repo archive is auto-detected** too.
18. **Persist the chosen settings** to a config file and pre-fill them on the next run.
19. **Auto-detect cargo**; if missing, give the user clear install instructions.
20. **Save all build log output** so failures can be read for troubleshooting.
21. **Time-measure every operation** to surface bottlenecks and guide performance work.
22. **Detach & reattach.** The user can quit the program and leave the builds running
    autonomously; reopening shows the live stats of what's going on.

---

## What it does

For every family in a `google/fonts` clone that has buildable source metadata, the
harness:

1. Resolves the upstream repo + pinned commit from `METADATA.pb`.
2. Streams the **pristine tree at that commit** out of the repo's bare mirror with
   `git archive` (a read-only operation — see guarantees below) into a throwaway
   extraction directory.
3. **Pre-cleans** any committed build outputs (`fonts/`, `*_ufo/`, `variable/`, …)
   from that throwaway tree so the build regenerates everything from sources.
4. Resolves the build config (a google/fonts **override** `config.yaml`, copied into
   the extracted repo root; else the in-repo `config_yaml`; else an auto-discovered
   `sources/config.yaml`).
5. Builds it — **Rust first** (`fontc`), falling back to Python (`fontmake`) — using
   the right **dependency cohort** venv.
6. Collects the built fonts + a full build log into the **separate build directory**,
   optionally `sha256`-compares them to the shipped binaries, then deletes the
   throwaway tree to reclaim space.

### Archive-safety guarantees (strict)

- **Sources are read only with `git archive <commit>` from the bare mirrors.** No
  checkout, no fetch into a working tree, no write of any kind into a mirror.
- **Archives are never deleted** and never modified. (Missing repos can be *added*
  with `--mirror-missing`, which only ever clones new mirrors — append-only.)
- **Every asset is written under `--build-dir`**, never inside a source repo.
- **Every build is from scratch** — a fresh extraction, output dirs pre-cleaned, the
  extraction discarded afterwards.

---

## Where the worklist comes from (`--source`)

Two data sources for the worklist/stats, both first-class:

- **`--source metadata`** (default) — the worklist and each family's pinned `commit`,
  config, and shipped binaries are read from a **google/fonts clone**'s `METADATA.pb`.
  This builds *exactly what Google Fonts ships* and can compare against the shipped
  binaries (`--compare`). Requires `--google-fonts`.
- **`--source archive`** — the worklist is **every bare mirror in the archive**, each at
  `--archive-rev` (default `HEAD` = the default-branch tip). The repo URL comes from the
  mirror's origin; the config is auto-discovered in the repo; `--google-fonts` is
  **optional** (no shipped binaries exist to diff against, so `--compare` is unavailable).
  This inspects/builds the *upstream sources as they are*, independent of what GF pins.

In **both** modes the **sources** always come from the archive (read-only `git archive`),
and `requirements.txt` for cohorts is read from the archive. `--source` only changes where
the *worklist* is enumerated.

## Build backends — Rust first, Python cohorts as fallback

The compiler backend is selectable with `--backend {auto,fontc,fontmake}`
(default `auto`):

- **`fontc` (Rust, preferred).** Runs `gftools.builder <config> --experimental-fontc
  <fontc-bin>`: gftools-builder orchestrates the recipe, but the actual compile is
  done by **fontc** (Rust) instead of fontmake. Provide the binary with `--fontc-bin`
  (build it once with `cargo build --release -p fontc` in a `googlefonts/fontc`
  checkout).
- **`fontmake` (Python, fallback).** The classic `gftools.builder <config>` path,
  used when fontc isn't available or a family fails under fontc.
- **`auto`** tries **fontc first** and falls back to fontmake, recording **which
  backend actually built each family**. That per-family record is the
  **Rust-migration metric**: it tells us what fraction of the library already builds
  with pure-Rust compilation, and which families still need Python.

> Note: even the fontc path currently drives the build through `gftools.builder`
> (Python orchestration). "Reduce reliance on Python" today means replacing the
> *compiler* (fontmake → fontc); the long-term goal is an all-Rust build with no
> Python in the loop. The harness is structured so the backend abstraction can later
> point at a fully-native Rust builder.

---

## Dependency cohorts (shared virtual environments)

Installing a venv per family would be wasteful; one giant venv risks conflicts. So
with `--manage-venvs` the harness groups families by their **build dependency set**:

- A family's cohort key is the hash of its repo `requirements.txt` (normalized:
  comments/whitespace stripped, sorted). Families with no/standard requirements share
  the **`base`** cohort.
- One venv is created per distinct cohort, under `--build-dir/venvs/<key>/`, with a
  shared pip cache (`--build-dir/pip-cache`). The first family that needs a cohort
  creates it (other workers wait on a per-cohort lock); everyone else reuses it.
- The `base` cohort venv is created once up front (from `--base-requirements`, the
  pinned GF toolchain) so workers don't stampede on startup.

Without `--manage-venvs`, all builds use a single interpreter you point at with
`--build-python`.

**Preview the grouping first** with `--cohorts-report`: it scans every family's
`requirements.txt` **read-only** (`git show` on the mirror — no extraction, no builds,
archives untouched) and prints how many distinct cohorts exist and which families fall
in each, plus a `cohorts.json`. Example (3% sample): 43 families → 17 cohorts, e.g. one
shared by 6 Noto families (notobuilder deps), one by 4 Playwrite families (a 138-package
pinned set), and 15 with no requirements in `base`.

---

## Parallelism & scheduling

- Builds run in a worker pool (`--jobs`, default = CPU count). Each build is an
  isolated subprocess (gftools.builder `chdir`s globally, so isolation matters —
  hence one process per build).
- **Longest-first scheduling** shrinks the tail: on a resumed run the queue is ordered
  by each family's previously-recorded build time (descending); on a first run a
  heuristic (variable fonts and multi-file families first) is used.
- A per-cohort lock means a given cohort venv is installed exactly once even under
  full parallelism.

---

## Modular, optional frontends

The build **core** (`Orchestrator`) is UI-agnostic. It exposes a `snapshot()` and
continuously writes two machine-readable files under `--build-dir`:

- `state.json` — the full resumable state (every family's status, backend, duration).
- `events.jsonl` — an append-only stream of `started` / `built` / `failed` / `venv`
  events.

A frontend just observes the core. Built-in frontends, chosen with `--ui`:

| `--ui` | Frontend | For |
|--------|----------|-----|
| `curses` | ncurses dashboard | interactive terminal |
| `plain`  | one line per completion + periodic summaries | logs, CI, non-TTY |
| `json`   | newline-delimited JSON snapshots to stdout | piping to other tools |
| `none`   | silent (state/events files only) | embedding |
| `auto` (default) | curses on a TTY, else plain | — |

**ncurses is never required.** Write your own frontend by subclassing `Frontend`, or —
for a **web UI** — run with `--ui none` (or `json`) and have your server tail
`events.jsonl` / poll `state.json` out-of-process. Nothing in the core imports curses
unless the curses frontend is actually selected.

## Partial runs (validation)

`--percent P` builds only an **evenly-spaced sample** of `P`% of the library (spread
across the alphabetical family list, so 5% still spans many foundries rather than one
corner). Ideal for validating the tool end-to-end before committing to a full run.
`--only ofl/a,ofl/b` picks an explicit subset.

## Phases & the live dashboard (TUI)

A run drives a background pipeline through phases, each reported live:
**`archive`** (mirror any missing upstream repos) → **`cohorts`** (scan/generate the
dependency cohorts) → **`build`** → **`done`**. The ncurses dashboard shows a phase banner +
progress bar and is **navigable** — three views switchable with `1`/`2`/`3` or `Tab`:

```
 Google Fonts library build                                   elapsed 01:23:45
 disk +12.3GiB  free 290.0GiB  jobs 8  cohorts 28  fontc 980/fontmake 240
 Phase: building   built 412/1503  failed 7  building 8  queued 1076
 [######################------------------------------------]  31%
  1 overview   2 cohorts   3 failures               [tab]switch [↑↓]scroll
 Now building ----------------------------------------------------------------
  w 1 ofl/notosanstc                       02:10  fontc
  w 2 ofl/roboto                           00:42  fontmake
 Recent failures (7) --------------------------------------------------------
  ofl/foldit                       gftools.builder exit 1: KeyError 'instances'
 [q]uit [p]ause   logs: <build-dir>/logs
```

- **overview** — build dashboard (now-building + recent failures).
- **cohorts** — the dependency cohorts, live and scrollable (largest first).
- **failures** — all failures, newest first, scrollable.

Keys: `1`/`2`/`3` or `Tab` switch views, `↑`/`↓` scroll, `p` pause/resume, `q` quit (lets
in-flight builds finish). During the `archive`/`cohorts` phases the banner shows that
phase's progress (e.g. `Phase: populating archive  340/1320`). Full per-family logs are at
`<build-dir>/logs/<slug>.<backend>.log`. For non-interactive use pick `--ui plain`
(prints phase transitions + progress), `--ui json`, or `--ui none`.

---

## Setup on your laptop

Prerequisites (native, **outside** the VM):

1. **Python 3.8+** (for the harness itself — pure standard library, no install).
2. A **base build venv** with the GF toolchain (see `requirements-build.txt`):
   ```sh
   python3 -m venv ~/gflib-venv
   ~/gflib-venv/bin/pip install -r requirements-build.txt
   ```
3. **fontc** (Rust compiler), for the Rust path:
   ```sh
   git clone https://github.com/googlefonts/fontc && cd fontc
   cargo build --release -p fontc        # binary at target/release/fontc
   ```
4. A **google/fonts clone** (for METADATA + the shipped binaries to compare against).
5. The **repo archive** of bare mirrors, laid out as `<archive>/<owner>/<repo>.git`.
   Copy it from the shared storage, or let `--mirror-missing` clone what's absent.

## Usage

```sh
# Preview the worklist:
python3 gflib_build.py --list \
  --google-fonts ~/google/fonts --archive ~/repo_archive --build-dir ~/gfbuild

# Preview the dependency-cohort grouping (read-only, no builds):
python3 gflib_build.py --cohorts-report \
  --google-fonts ~/google/fonts --archive ~/repo_archive --build-dir ~/gfbuild

# Full library build, Rust-first, with cohorts and the live TUI:
python3 gflib_build.py \
  --google-fonts ~/google/fonts \
  --archive     ~/repo_archive \
  --build-dir   ~/gfbuild \
  --backend auto --fontc-bin ~/fontc/target/release/fontc \
  --manage-venvs --base-python python3 --base-requirements requirements-build.txt \
  --jobs 8 --compare --mirror-missing
```

```sh
# Validate the tool on 5% of the library first, plain output:
python3 gflib_build.py --google-fonts ~/google/fonts --archive ~/repo_archive \
  --build-dir ~/gfbuild --percent 5 --ui plain --compare
```

Useful flags: `--percent 5` (sample), `--only ofl/dmsans,ofl/roboto` (subset),
`--ui {curses,plain,json,none}`, `--retry-failed`, `--rebuild` (ignore prior state),
`--discard-fonts` (keep only the comparison result, not the built binaries),
`--keep-work` (debug: keep the extraction).

## Detached builds, monitoring & persistence

- **Run autonomously, watch later.** `--detach` runs the build in a background daemon and
  opens a live monitor; **quit the monitor with `q` and the build keeps running.** Reattach
  any time with `--attach --build-dir <dir>` (read-only live view), or stop it with
  `--stop --build-dir <dir>` (graceful). The daemon writes `status.json` every ~1 s, which
  the monitor (and any external tool) reads.
- **Persisted settings.** Your wizard/CLI choices are saved to
  `<data-dir>/gflib-build.config` and pre-fill the next run (CLI flags still override;
  `--no-save-config` to skip, `--config` for a custom path).
- **Full logs for troubleshooting.** Every family gets a complete `logs/<slug>.log` — a
  timestamped pipeline narrative (mirror → extract → venv → config → build) **plus the full
  `gftools.builder` output** of every backend attempt — kept whether it succeeds or fails.
- **Everything is timed.** Each operation (mirror/extract/venv/config/build/collect/compare)
  and each phase is measured into `timings.json`, printed as a summary at the end, and shown
  live in the **stats** view (`4`) — to find and shrink bottlenecks over time.

## Outputs, state & resumability

Under `--build-dir`:

```
state.json            resumable per-family status (built/failed/…) + durations
status.json           live full snapshot (read by the monitor / external tools)
timings.json          per-phase + per-operation timing (bottleneck analysis)
out/<slug>/           built fonts (omit with --discard-fonts)
logs/<slug>.log       complete per-family log: pipeline narrative + gftools output
events.jsonl          append-only event stream (started/built/failed/venv)
daemon.pid, daemon.log   present when run with --detach
venvs/<cohort>/       shared cohort virtualenvs (with --manage-venvs)
pip-cache/            shared pip download cache
work/<slug>/          throwaway extraction (deleted after each build)
```

Re-running resumes: already-built families are skipped; `--retry-failed` re-attempts
failures; `--rebuild` starts over.

## Roadmap

- [x] Archive-safe pristine extraction; separate build dir; never touch/delete archives.
- [x] Live TUI (A–E) + headless mode; resumable state.
- [x] Parallel worker pool + longest-first scheduling.
- [x] Rust-first backend selection (`fontc`) with Python (`fontmake`) fallback + per-family backend record.
- [x] Dependency cohorts sharing venvs.
- [x] Modular, optional frontends (curses / plain / json / none) + state.json/events.jsonl for external/web UIs.
- [x] Partial runs via `--percent` (evenly-spaced sample).
- [x] Two worklist sources: `--source metadata` (google/fonts) and `--source archive` (the mirrors directly).
- [x] Zero-to-built bootstrap: auto-clone google/fonts + populate the archive, behind a setup wizard (`--yes` to skip); default paths under `--data-dir`.
- [x] Phase-driven pipeline (archive → cohorts → build) with a phase-aware, navigable TUI (overview / cohorts / failures / stats).
- [x] Editable ncurses setup wizard (cursor, conditional fields, ±5 stepping, fontc/cargo/archive auto-detect); persisted to a config file.
- [x] Detach/reattach: run the build as a daemon (`--detach`), monitor live (`--attach`), stop gracefully (`--stop`).
- [x] Comprehensive per-family logs + per-operation/per-phase timing (`timings.json`, stats view).
- [ ] Emit a migration report: % of library building under fontc, and the blockers per family.
- [ ] Feed results back to the gfonts_agents dashboard (`reproducible_build` + provenance levels).
- [ ] Track toward an **all-Rust** build with no Python orchestration in the loop.

---

*This tool and README were assisted by an AI agent (Claude Opus 4.8) under the
guidance of @felipesanches.*
