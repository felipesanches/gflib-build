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

With no existing data, a single command bootstraps everything. On first run it opens the
**Configuration tab** (there is no separate "wizard") to set up your build, then drives the
whole pipeline live in the TUI:

```sh
python3 gflib_build.py            # uses ./gflib-data/ for everything
```

The Configuration tab is an **interactive ncurses form** with the settings pre-populated to
their current values — you edit any field, watch the live "Plan" update, then **▶ Start**:

```
 Google Fonts library build — Configuration                       first-time setup
  1 config   2 overview   3 cohorts   4 failures   5 stats   (others appear after ▶ Start)
 set up your build, then ▶ Start   [↑↓/Tab]move  [space]toggle  [←→]cursor/step/cycle  …

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
**▶ Start build** and press Enter (or `Esc`/Cancel). Then it runs the pipeline — **clone →
populate archive + build (streaming)** — live, landing on the Configuration tab. `--yes` skips
the Configuration screen, `--wizard` forces it; `C` returns to it from the live dashboard.

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
16. **Configuration screen.** It must **ask the user before** doing those heavy things
    (cloning, mirroring) — via the Configuration tab, not silent. (No separate "wizard": the
    first-run setup screen *is* the Configuration tab.)
17. **The Configuration screen is an editable ncurses form** with fields pre-populated with the default
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
23. **The entire interaction is ncurses.** The bootstrap operations (clone fontc, build
    fontc, clone google/fonts, populate archive, scan cohorts, build) all show up as a live
    **task-list with emoji status** (✅ done · 🔄 running · ⏳ pending · ❌ failed · ➖ n/a),
    plus per-task percentage and elapsed time — not plain CLI prints.
24. **Quit frees the shell; resume is instant.** Pressing `q` returns to the shell with the
    build still running; re-running the program (from the same or a different terminal)
    reattaches straight to live updates **without showing the setup interface again**.
25. **Live archive list.** As repos are populated into the archive, their names appear in a
    gradually growing list in the UI.
26. **Arrow-key tabs.** The dashboard's view tabs are driven by the ←/→ arrows (number keys
    still jump directly).
27. **Now-building shows checkouts.** A family appears in the "Now building" list (with a
    `checkout` tag) from the moment its source is being extracted, not only once it compiles.
28. **List selection + detail.** On every tab, `↑`/`↓` select an item in that tab's list and
    `↵` opens a detail overlay (a failure's full error + log tail, a cohort's requirements,
    etc.); `Esc`/`←`/`↵` returns.
29. **Cumulative clock.** The elapsed timer represents the real time spent so far across
    reopen/resume — it is not reset to zero when the program is reopened.
30. **`C` returns to the setup wizard** from the live dashboard (to change settings / start
    over); the running build is replaced only if the user actually starts a new one.
31. **Dynamic, streaming pipeline (no barriers).** Mirroring, cohort assignment, and building
    run concurrently: a repo is evaluated for its cohort and built the moment it's available
    in the archive — the build does not wait for "populate archive" to finish. An archive
    pre-warmer mirrors ahead using idle I/O; a shared per-repo clone lock means no repo is
    cloned twice; clones are abortable so shutdown never blocks. The archive list grows live.
32. **Configuration tab + LIVE config changes.** A "config" tab shows the live settings and
    edits them; applying takes effect on the RUNNING build with no restart — raising percent
    fetches/cohorts/builds the newly-included families, raising jobs starts more parallel
    workers (via a `control.json` the daemon polls). Drive config from one monitor at a time.
33. **Self-healing dependencies — no manual pin management.** If a venv's `pip install` can't
    satisfy a pinned version (a stale/dev pin absent from PyPI), the installer automatically
    drops just that pin, lets pip backtrack to a compatible version, retries, and records the
    relaxation in the config tab. Valid pins are kept, so reproducibility holds for everything
    that resolves; the user never has to hand-edit `requirements-build.txt` to unblock a build.
34. **Config tab is leftmost + the default view**, reflecting the current settings (falls back
    to the persisted config so it never shows a list of `None`).
35. **`--reset` completely cleans the system** — deletes all built assets + virtual environments
    (the whole build dir); the repo archive is NEVER touched (strict append-only policy).
36. **No separate wizard — the Configuration tab is the setup.** First-run setup is the
    Configuration tab itself (dashboard chrome, all fields editable, ▶ Start), and the same tab
    in the live dashboard does live edits. One config interface, not two.
37. **One Configuration tab, fully editable in place.** ←/→ move the text cursor / ±step /
    cycle, type to edit — no reloading a separate screen to edit. Tab navigation is `Tab` /
    `Shift-Tab` only (so ←/→ and number keys are free for editing).
38. **A "built" tab** lists the successfully built families (successes), parallel to Failures.
39. **Section navigation** — multi-section tabs (overview, stats) are navigable section by
    section: ←/→ focus a section (the ▼-marked one), ↑/↓ navigate its items, ↵ acts on the
    selected item. (Ctrl+Tab is intercepted by most terminals, so ←/→ is the binding.)
40. **Always-visible status panel.** A panel just above the footer instantly shows the most
    useful info about whatever item is focused, anywhere in the program — a single line in most
    cases (status-bar style), growing to a few lines when there's more worth showing. The
    motivating case: a red `✗` entry in *Overview → Archive — mirrored* now explains *why* that
    repo could not be mirrored into the archive.
41. **Header shows total build-system disk usage**, not a per-session delta. The old
    `disk +0B`-on-reopen was useless; the header now reports the whole build dir's on-disk size.

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

## Pipeline task-list & the live dashboard (TUI)

The **entire interaction** is ncurses — not just the builds. A run drives a background
pipeline rendered as a live **task-list** with emoji status (✅ done · 🔄 running · ⏳
pending · ❌ failed · ➖ n/a) plus per-task progress and elapsed time:
**`clone google/fonts`** → **`build fontc`** → **`discover worklist`** → then **`populate
archive`** and **`build fonts`** running **concurrently** (mirroring, cohort assignment and
compiling all stream together — no barriers) → **`done`**. The dashboard also shows a phase
banner + progress bar and is **navigable** — four views switchable with the **←/→ arrows**
(or `Tab`):

```
 Google Fonts library build                                   elapsed 01:23:45
 build dir 142.8GiB  free 290.0GiB  jobs 8  cohorts 28  fontc 980/fontmake 240
 Phase: building   built 412/1503  failed 7  building 8  queued 1076
 [######################------------------------------------]  31%
  config  overview  cohorts  built  failures  stats        [Tab]/[⇧Tab] switch tabs
 ▼ Pipeline (6) -------------------------------------------------------------
  🔄 build fonts        612/1503  40% 03:12  (mirror + cohort + compile, streaming)
 ▷ Archive — mirrored (340) -------------------------------------------------
  + google/fonts-sources    ✗ owner/dead-repo
 ▷ Now building (8) ---------------------------------------------------------
  w 1 ofl/notosanstc                       02:10  checkout
 ▷ Recent failures (7) ------------------------------------------------------
  ofl/foldit                       gftools.builder exit 1: KeyError 'instances'
 ───────────────────────────────────────────────────────────────────────────
  ✗ owner/dead-repo — could NOT be mirrored into the archive
    remote: Repository not found (HTTP 404)
 [Tab]tabs  [←→]section  [↑↓]item  [↵]details  [C]onfig  [q]uit
```

Tabs (switch with **`Tab` / `Shift-Tab` only**):
- **config** (leftmost, default) — the ONE Configuration tab, used for both first-run setup
  and live editing (no separate wizard). `↑`/`↓` pick a field; **`←`/`→` move the text cursor /
  ±step a number / cycle a choice**; type to edit; `space` toggles. On a running build, the
  live-editable fields (percent, jobs, backend, timeout, compare, populate) apply with **✓ apply
  changes** — raising **percent** immediately fetches+cohorts+builds more families, raising
  **jobs** spawns more workers; path/source fields show `(restart: C)`. First run shows the same
  tab with **▶ Start build / Cancel**.
- **overview** — the pipeline task-list + the live archive list + now-building + recent failures,
  as navigable **sections**.
- **cohorts** — the dependency cohorts, live (largest first).
- **built** — the list of **successfully built** families (newest first; ↵ shows output path,
  size, vs-shipped comparison + rebuild command).
- **failures** — all failures, newest first.
- **stats** — fontc-migration tally + per-phase / per-operation timing (sections).

Keys: **`Tab`/`Shift-Tab`** switch tabs; within a tab **`←`/`→` move focus between sections**
(the `▼`-marked one), **`↑`/`↓` navigate items** in the focused section, **`↵` open a detail
overlay** (a failure's error + log tail, a built family's output, a cohort's requirements, a
task/op's detail; `Esc`/`↵` returns); `C` opens/returns to the Configuration tab, `q` quits.
(`Ctrl+Tab` for sections is intercepted by most terminals — e.g. gnome-terminal's own tab
switching — so `←`/`→` is the reliable binding.) The **elapsed clock is cumulative** across
reopen/resume. Full per-family logs are at `<build-dir>/logs/<slug>.log`. For non-interactive
use pick `--ui plain`, `--ui json`, or `--ui none`.

An **always-on status panel** sits just above the footer and explains the **currently focused
item** wherever you are — a one-liner that grows to 2–3 lines only when there's more worth
showing. Move focus with `←`/`→` (sections) and `↑`/`↓` (items), or `↑`/`↓` over the
Configuration fields, and the panel updates instantly: what a config field does, what `▶ Start`
will launch, why a build is failing, what a cohort needs — and, for a **red `✗` entry in
Archive — mirrored**, *why* that repo could **not** be mirrored (e.g. `Repository not found
(HTTP 404)`). The header's **`build dir <size>`** is the *total* on-disk footprint of the whole
build system (`out/`, `venvs/`, `work/`, logs), recomputed periodically — not a per-session
delta — so it's meaningful the moment you reopen the dashboard.

### Quit anytime — the build keeps running, resume straight to live updates

A fresh interactive (curses) build runs **detached by default**: it builds in a background
daemon while you watch a live monitor. Press **`q`** and you're back at the shell with the
build still running — go do anything else. **Re-run the program** (from the same terminal or
a different window) and it detects the running build and **reattaches the live monitor,
skipping the setup wizard entirely** — straight back to live updates. `--stop` cancels the
build gracefully. (`plain`/`json`/`none` UIs stay in the foreground for scripting/logging;
`--detach` forces detach for any UI.)

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

```sh
# Completely reset — delete ALL built assets + virtual environments (the whole build dir) to
# start clean. The repo archive is NEVER touched (strict append-only policy); the google/fonts
# clone is kept too. Refuses if a build is running (--stop first) or if the archive is inside
# the build dir. Add --yes to skip the confirmation.
python3 gflib_build.py --reset
```

```sh
# Rebuild ONE family (e.g. after fixing a config) — --only restricts the WHOLE pipeline
# (mirror/cohorts/build) to just that family, --rebuild ignores its prior state, --yes skips
# the wizard. (The Failures tab's detail overlay shows this exact command for each failure.)
python3 gflib_build.py --only ofl/dmsans --rebuild --yes
```

The **build-fonts task reports its outcome**, not just "processed": its detail shows
`N built, M failed`, and it turns ❌ (not ✅) if every build failed — so the task-list never
looks like a success when nothing built. (The archive task counts unreachable repos
separately as `unreachable`, distinct from build failures.)

## Detached builds, monitoring & persistence

- **Run autonomously, watch later — by default.** A fresh interactive (curses) build
  detaches automatically: it runs in a background daemon and opens a live monitor. **Quit the
  monitor with `q` and the build keeps running.** **Just re-run the program** to reattach
  (it detects the running daemon and skips the wizard), from this terminal or any other — or
  use `--attach --build-dir <dir>` explicitly. Stop it with `--stop --build-dir <dir>`
  (graceful). `--detach` forces this for non-curses UIs too. The daemon writes `status.json`
  every ~1 s (atomic rename), which the monitor (and any external tool) reads.
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
- [x] End-to-end ncurses task-list (clone gf → build fontc → discover → archive → cohorts → build) with emoji status, per-task %/elapsed, a live archive-population list, and ←/→ tab navigation.
- [x] Editable ncurses setup wizard (cursor, conditional fields, ±5 stepping, fontc/cargo/archive auto-detect); persisted to a config file.
- [x] Detach-by-default + auto-attach: `q` frees the shell with the build still running; re-running reattaches live (any terminal) without the wizard; `--attach`/`--stop` explicit; `--detach` for non-curses UIs.
- [x] Comprehensive per-family logs + per-operation/per-phase timing (`timings.json`, stats view).
- [x] Migration tracking: per-family backend record + `--backend both` comparison; `migration.json` (fontc / fontmake-fallback blockers / both-identical-or-differ) + stats view + end summary.
- [ ] Feed results back to the gfonts_agents dashboard (`reproducible_build` + provenance levels).
- [ ] Track toward an **all-Rust** build with no Python orchestration in the loop.

---

*This tool and README were assisted by an AI agent (Claude Opus 4.8) under the
guidance of @felipesanches.*
