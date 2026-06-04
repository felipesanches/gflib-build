# gflib-build (Rust — the official implementation)

The official implementation of `gflib-build`. It began as a from-scratch Rust port of the original
Python `gflib_build.py`; that port reached full parity and **Rust is now canonical** — the Python
tool has been removed (it remains in git history for reference). An
**archive-safe** harness that builds the **entire Google Fonts library** locally, with a **live TUI**,
a **web dashboard**, a **parallel build engine**, **resumable state**, and **M0 provenance**
(record which compiler *and* which orchestrator built — or attempted — every family).

> Status: **v0.1 — compiles, runs, and is schema-compatible with the Python tool.** The core build
> pipeline, both UIs, discovery, persistence, live control, and provenance work today. Some Python
> features are simplified or not yet ported — see [Parity & gaps](#parity--gaps).

## Why a Rust port

Spec 12 (the long-term goal) is an **all-Rust** Google Fonts build with no Python in the loop. The
Python harness is the reference; this port is the first step toward making the *harness itself* Rust,
independent of the per-family build backend. Felipe asked for both to coexist so they can be tested
head-to-head and a direction chosen.

## Build & run

```sh
cd rust
cargo build --release           # binary at target/release/gflib-build
cargo test                      # 18 unit tests (model serde, provenance, discovery, parsing, …)

# preview the worklist (same numbers as the Python tool):
./target/release/gflib-build --list --source metadata --google-fonts /path/to/google/fonts

# build a 1-family sample headless, with full M0 provenance recorded:
./target/release/gflib-build --source archive --archive /path/to/repo_archive \
    --only owner/repo --ui none --jobs 1

# live TUI (default on a terminal):
./target/release/gflib-build --source archive --archive /path/to/repo_archive --percent 2

# web dashboard:
./target/release/gflib-build --attach --ui web --web-port 8765 --build-dir gflib-data/build
```

Dependencies are deliberately minimal — `serde`, `serde_json`, `crossterm` only. The web server is
hand-rolled on `std::net` (no HTTP crate); the CLI parser is hand-rolled (no clap). This keeps the
build fast and the supply chain small.

## Interoperability with the Python tool (important)

The on-disk schema is **byte-compatible**: `status.json`, `state.json`, and `control.json` use the
same field names. So:

- **The Rust TUI / web UI can monitor a Python daemon** — point `--attach --build-dir <dir>` at a
  build started by `python3 gflib_build.py`, and it renders that daemon's `status.json` live.
- **The Python monitor can watch a Rust build** — and vice-versa.
- **Live controls cross the boundary** — a control written by either UI (`jobs` / `percent` /
  `pause` / `retry`) is applied by either daemon (the Rust `ControlSet` omits unset keys, so a
  Python daemon never sees a `null`).
- **M0 provenance is preserved end-to-end** — `compiler_version`, `builder`, `builder_version` round
  trip through both ports' JSON.

This is what makes the two genuinely comparable: you can run the *same build* and swap UIs, or run
one port's build under the other port's dashboard.

## Architecture (module map)

| Module | Responsibility | Python counterpart |
|--------|----------------|--------------------|
| `model.rs` | serde types = the JSON schema (Snapshot, Res, Family, ControlSet) | the dicts `snapshot()` emits |
| `util.rs` | `human`/`hms`, dir sizing, free space | module helpers |
| `provenance.rs` | **M0** compiler + builder version strings | `compiler_version_str` / `builder_version_str` |
| `discover.rs` | metadata + archive worklist discovery, `--percent` sampling, fontc/archive auto-detect | `discover` / `discover_from_archive` |
| `config.rs` | CLI parsing + config-file persistence | `build_argparser` / `load_config` |
| `persist.rs` | status/state/control/failure-history/pidfile IO (atomic) | the `*_path` + write helpers |
| `build.rs` | the `Orchestrator`: worker pool, extract→config→build→collect, provenance, control | `Orchestrator` |
| `monitor.rs` | read-only `MonitorState` (mtime-gated) + the `Source` trait | `MonitorState` |
| `tui.rs` | crossterm dashboard (tabs, selection, detail, status panel) | `CursesFrontend` |
| `web.rs` | std HTTP dashboard (`/`, `/api/status`, `/api/control`) | `WebFrontend` |

The build core is UI-agnostic: both frontends render the `Source` trait (`snapshot()` + `control()`),
implemented by the live `Orchestrator` and by `MonitorState`.

## What's verified working

- `--list` produces the **exact same counts** as the Python tool (1423 buildable / 2013 in library
  / 590 skipped on the current library).
- A real 1-family build runs the **full pipeline** — `git archive` extraction (read-only), config
  resolution, `gftools.builder` invocation, output collection — and records the failure (here: no
  gftools in the env) **with M0 provenance** in `state.json` *and* the append-only
  `failure-history.jsonl`.
- The web UI serves the page, `/api/status`, and `/api/control` (writes `control.json`).
- The Rust monitor renders a **Python-written** `status.json`, provenance fields intact.
- `cargo test`: 18 green.

## Parity & gaps

**Ported and working:** archive-safe pristine extraction; separate build dir; never touch/delete
archives; parallel worker pool; fontc-first / fontmake-fallback / `both` / builder3 backend order;
resumable `state.json` + reconciliation; durable `failure-history.jsonl`; failure classification +
hints; `--percent` even sampling; `--only`; both worklist sources; live `control.json` (jobs /
percent / pause / retry / retry-all) with clamping; status writer; disk accounting (build + archive,
no double-count); fontc/archive auto-detect; TUI (8 tabs, selection, detail overlay, status panel,
progress bar) and web dashboard; **M0 compiler + builder provenance** (the recent focus) on success
and failure; `--list` / `--attach` / `--stop` / `--reset`.

**Simplified or not yet ported (documented, not hidden):**

- **Dependency cohorts / venv management.** The Python tool groups families by `requirements.txt`
  into shared venvs (`--manage-venvs`). The Rust port currently builds with a single interpreter
  (`--build-python`, the common default path). Cohort fields exist in the schema but aren't populated.
- **`--backend both` comparison.** The order is honoured but the sha256 vs-table isn't computed yet.
- **Archive populate / mirror pre-warmer.** ✅ ported: `--mirror-missing` clones missing repos, and a
  concurrent pre-warmer proactively mirrors the whole worklist (de-duped) into the archive, populating
  the live archive view (cloning-now / queued / recently-added / unreachable). Build workers and the
  pre-warmer share the per-repo clone lock so nothing is cloned twice.
- **`build_rules.json` pre-build scripts.** Not yet executed.
- **True detach/daemonize.** A live build runs in the foreground; quitting the TUI stops it (the
  Python tool double-forks a lingering daemon). Monitoring an *external* daemon already works via
  `--attach`. The pidfile/`--stop` plumbing is in place for when daemonize lands.
- **Config tab editing & setup wizard.** ✅ ported: the TUI config tab is a full CONFIG_SCHEMA
  editor (14 fields, show_if visibility, live-apply via `control.json`), and `--setup`/`--wizard`
  (or a missing google/fonts clone) opens it pre-build as the first-run wizard, launching on
  ▶ Start build.
- **`--compare`, `--cohorts-report`, `--retry-category` UI**, self-healing dependency relaxation,
  longest-first scheduling: partial or pending.

None of these gaps affect the **archive-safety invariants** (read-only `git archive`, append-only
archive, all output under the build dir) or the **M0 provenance** guarantee, which are fully ported.

---

*This port was written by an AI agent (Claude Opus 4.8) under the guidance of @felipesanches.*
