# Architecture

`gflib-build` is a single-file, pure-stdlib Python program (`gflib_build.py`) that
orchestrates building every buildable family in the Google Fonts library from pristine
archived sources, and renders progress through a pluggable frontend. The actual font
compile is delegated to a separate interpreter/venv (gftools.builder + fontmake) and/or
the `fontc` binary.

## Module map (sections within `gflib_build.py`)

| Section | Responsibility |
|---|---|
| `Family`, `Result` | dataclasses: the work item and its evolving state |
| discovery | `parse_metadata`, `discover` (METADATA-driven), `discover_from_archive` (mirror-driven, `--source archive`), `sample_evenly` — build the worklist, sample for `--percent` |
| mirror/git | `mirror_path`, `git`, `ensure_mirror`, `extract_tree`, `preclean_outputs` — archive-safe source access |
| bootstrap | `ensure_google_fonts` (shallow-clone if absent), `populate_archive` (parallel mirror-missing, append-only), `scan_cohorts`, `setup_wizard` (editable ncurses form), `detect_fontc`/`detect_archive`/`detect_cargo`, `build_fontc_from_source` |
| detach/monitor | `daemonize` (double-fork), `read_daemon_pid`, `MonitorState` (read-only view from `status.json`), `run_monitor`; `Orchestrator._status_writer` writes `status.json` every ~1 s |
| persistence/timing | `load_config`/`save_config` (`gflib-build.config`); `Orchestrator._record_op`/`phase_durations`/`write_timings` (per-op + per-phase timing → `timings.json`); per-family log `logs/<slug>.log` (pipeline narrative + full gftools output) |
| config | `resolve_config`, `read_requirements`, `normalize_requirements`, `cohort_key_for`, `read_requirements_from_mirror` |
| `VenvManager` | dependency cohorts: one shared venv per distinct requirements set |
| building | `run_builder` (backend-aware), `collect_outputs`, `sha256`, `compare_to_shipped` |
| `Orchestrator` | the **UI-agnostic core**: queue, worker pool, state, events, scheduling |
| frontends | `Frontend` base + `Curses/Plain/Json/None` + `FRONTENDS` registry + `pick_frontend` |
| report | `cohorts_report` — read-only cohort preview |
| main | argument parsing and wiring |

## Pipeline task-list (`Orchestrator._drive`, background thread)

`run()` starts a single background **driver** thread that walks the run through an
end-to-end **task-list** — so the *entire* interaction (not just the builds) renders live
in the UI. Each step is a `Task` (`key/name/status/t0/t1/done/total/detail`) exposed via
`snapshot()["tasks"]`; the active step also drives the legacy `phase`/`phase_done`/
`phase_total`/`phase_label` fields (for the banner + per-phase timing):

1. **`clone_gf`** — `ensure_google_fonts()` shallow-clones google/fonts if the worklist
   needs METADATA and no clone is present (skipped otherwise).
2. **`build_fontc`** — `build_fontc_from_source()` (`cargo build --release`) when the user
   opted in and no binary was found/given (skipped otherwise). Sets `args.fontc_bin`.
3. **`discover`** — `discover()` / `discover_from_archive()` + `sample_evenly()` build the
   worklist, then `_enqueue()` populates the queue.
4. **`archive`** (if `--populate-archive`) — `populate_archive()` mirrors any referenced
   upstream repo not already present (parallel; append-only; never mutates/deletes). Each
   newly-mirrored repo is appended to `self.archive_log` → `snapshot()["archive_recent"]`,
   a live, growing list rendered in the UI.
5. **`cohorts`** — `scan_cohorts()` reads each family's `requirements.txt` (read-only `git
   show`) and groups them; the live list is published in `self.cohorts`.
6. **`build`** — creates the base venv (if `--manage-venvs`), starts the worker pool, and
   waits until `all_done()`.
7. **`done`** — set in `_drive`'s `finally` (even on error: `phase_error` is recorded, and
   the in-flight task is marked `failed`), which also saves state and closes the events file.

`main()` only resolves paths, runs the **setup wizard**, and validates (fail-fast) *before*
the driver; every expensive/long step (clone, fontc, discover, mirror, cohorts, build) runs
inside the driver so it shows in the task-list. `join()` awaits the driver. Frontends treat
`phase == "done"` as the completion signal. Read-only paths (`--list`/`--cohorts-report`)
discover synchronously in `main()` (no driver/UI).

### Detach-by-default & auto-attach

A fresh interactive (curses) build **detaches by default** (`daemonize()`): the build runs
in a background daemon and the foreground process attaches a read-only **monitor**. Quitting
the monitor with `q` frees the shell while the build keeps running. Re-running the program
(from the same or any other terminal) detects the live daemon via `read_daemon_pid()` and
**auto-attaches the monitor — skipping the wizard entirely** — so you resume straight to live
updates. `--stop` cancels; `plain`/`json`/`none` UIs stay in the foreground for scripting.

## Per-family build pipeline (`Orchestrator._build_one`)

1. **`ensure_mirror`** — locate the bare mirror for the repo; confirm the pinned commit
   exists (a read-only `cat-file -e`, with a `remote update` retry if missing). With
   `--mirror-missing`, an absent repo is cloned `--mirror` (append-only; never deletes).
2. **`extract_tree`** — stream the committed tree at the pinned commit into
   `<build-dir>/work/<slug>` via `git archive | tar -x`. This is the only way sources
   are read: **read-only on the mirror, no checkout, no working tree in the mirror.**
3. **interpreter selection** — if `--manage-venvs`, read the extracted `requirements.txt`
   and obtain the cohort venv (`VenvManager.get_python`); else use `--build-python`.
4. **backend attempts** (`_backend_order`: `fontc` then `fontmake` for `auto`) — for each
   attempt: a fresh extraction on fallback, `preclean_outputs` (wipe committed
   `fonts/`, `*_ufo/`, `build*.ninja`, …), `resolve_config`, then `run_builder`.
5. **`collect_outputs`** — copy produced `.ttf`/`.otf` found under `FONT_SUBDIRS`
   (`work/fonts/{ttf,variable,otf}`, `work/fonts`, and the extraction root `work/`) into
   `<build-dir>/out/<slug>`, matched against the family's shipped filenames; record bytes
   and any missing shipped files (`out_missing`).
6. **`compare_to_shipped`** (with `--compare`) — sha256 the built vs shipped binaries →
   `identical` / `differ` / `missing`.
7. **cleanup** — a `try/finally` always removes `work/<slug>` (unless `--keep-work`);
   failures also drop partial `out/<slug>` debris. So nothing leaks and nothing is left
   inside any repo.

### Config resolution (`resolve_config`)

`gftools.builder` `chdir`s to the **config file's parent directory** and resolves
`sources:` relative to there. Therefore:

1. **google/fonts override** (`<google-fonts>/<slug>/config.yaml`): copied into the
   extraction **root** as `__gflib_override_config.yaml`, because override configs use
   repo-root-relative source paths.
2. **in-repo `config_yaml`** (from METADATA): used **in place** — its `sources:` are
   already relative to its own directory.
3. **auto-discovered** `sources/config.yaml` (etc.) as a fallback.

## Build backends

`run_builder` runs `python -m gftools.builder <config>` with `SOURCE_DATE_EPOCH=0` and
the interpreter's `bin/` prepended to `PATH` (gftools.builder shells out to
`fontmake`/`ninja`/`ttfautohint` **by name**). The Rust path adds
`--experimental-fontc <bin>`. `auto` tries `fontc` first and falls back to `fontmake`,
recording the backend that actually built each family — the **Rust-migration metric**.

## Dependency cohorts (`VenvManager`)

`cohort_key_for(requirements)` = `"base"` if empty, else `"c-" + sha1(normalized)[:12]`,
where normalization strips comments/whitespace and sorts lines. One venv per cohort under
`<build-dir>/venvs/<key>/`, created lazily under a **per-cohort lock** (so a venv installs
exactly once under parallelism), with a shared `pip-cache`. The `base` venv is created up
front. `_ready` (cohort → interpreter) is read/written under a single global lock.

## Concurrency model

- A `queue.Queue` of slugs feeds a pool of `--jobs` daemon worker threads. Each build is
  an isolated **subprocess**, so the GIL is released during the compile and threads
  suffice (also, `gftools.builder` does a global `os.chdir`, so process isolation is
  required for parallel correctness).
- Shared `Orchestrator` state (`results`, `failures`) is guarded by `self.lock`;
  `snapshot()` takes a consistent copy under it.
- `events.jsonl` writes are serialized by a dedicated lock.
- Termination: a worker exits when `all_done()` (every result terminal) **and** the queue
  is empty. `stop` (set by `join()`/the frontend loop once `all_done()`, by SIGINT, or by
  a frontend on quit) is also checked right after dequeue so no new build starts during
  shutdown.

## State & events (consumable by any frontend or external tool)

`<build-dir>/state.json` — full resumable state, written after every **terminal**
transition (built/failed) and at shutdown (the in-progress `building`/`started`
transition is recorded only in `events.jsonl`):
```json
{ "saved_at": <epoch>, "build_dir": "...",
  "results": { "ofl/dmsans": { "status": "built", "backend": "fontmake",
    "cohort": "c-...", "started": ..., "ended": ..., "out_bytes": ..., "out_missing": 0,
    "compare": "differ", "log": "logs/ofl__dmsans.fontmake.log", "config_used": "..." } } }
```

`<build-dir>/events.jsonl` — append-only stream, one JSON object per line:
```
{"t": 0.0,  "type": "started", "slug": "...", "worker": 1}
{"t": 1.3,  "type": "venv",    "slug": "...", "cohort": "c-..."}
{"t": 31.2, "type": "built",   "slug": "...", "backend": "fontmake", "bytes": ..., "compare": "differ", "missing": 0, "dur": 31.2}
{"t": 8.5,  "type": "failed",  "slug": "...", "error": "..."}
```

`Orchestrator.snapshot()` returns the live aggregate a frontend renders:
`elapsed`, `disk_used_delta`, `disk_free`, `jobs`, `paused`, `total`,
`counts{built,failed,building,queued,skipped}`, `backends{fontc,fontmake}`,
`building[{slug,worker,dur,backend,note}]`, `failures_recent[{slug,error,log}]`,
`cohorts_ready`, `done`.

## Archive-safety invariants (enforced by construction)

- The bare mirrors are read with `git archive` / `git show` / `cat-file` only. The sole
  writes git ever does to the archive are `clone --mirror` (new repos, `--mirror-missing`)
  and `remote update --prune` (updates refs; never removes commits). **No checkout, no
  gc, no delete.**
- Every byte the tool produces lives under `--build-dir`. Source repos are never written.
- Each build starts from a fresh extraction with committed outputs pre-cleaned.
