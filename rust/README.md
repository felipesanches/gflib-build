# gflib-build — source (Rust)

This directory is the `gflib-build` tool itself. For what the tool *is* and how to use it, start with
the [top-level README](../README.md); this file is for **building and hacking on the code**.

## Build & test

```sh
cargo build --release        # binary at target/release/gflib-build
cargo test                   # unit tests (serde round-trips, parsers, venv/cohort logic, …)
cargo run -- --help          # the full CLI surface
```

The dependency footprint is intentionally tiny — `serde`, `serde_json`, and `crossterm`, nothing
else. The web server is hand-rolled on `std::net` (no HTTP crate) and the CLI parser is hand-rolled
(no `clap`), to keep builds fast and the supply chain small.

A PATH launcher that always points at this repo's data dir:

```sh
./install-cli.sh             # writes ~/.local/bin/gflib-build (or pass a dir)
```

## Design in one paragraph

A background **daemon** (the `Orchestrator`) owns a build directory and writes an atomic
`status.json` snapshot ~once a second; it reads live commands from `control.json`. Every UI is a thin
**monitor** that renders a `snapshot()` and sends back a `ControlSet` — so the terminal UI, the web
UI, and any external tool all observe the same state through the same files, and exactly one daemon
owns a build dir at a time. The core is UI-agnostic: both frontends render the `Source` trait,
implemented by the live `Orchestrator` and by the read-only `MonitorState`.

## Module map (`src/`)

| Module | Responsibility |
|--------|----------------|
| `main.rs` | CLI entry; dispatches build / daemon / monitor / one-shot modes |
| `config.rs` | CLI parsing + config-file persistence (the editable `CONFIG_SCHEMA`) |
| `model.rs` | serde types = the on-disk JSON schema (`Snapshot`, `BuiltItem`, `ControlSet`, …) |
| `build.rs` | the `Orchestrator`: worker pool, extract → config → build → collect, live control, packaging worker |
| `discover.rs` | worklist discovery (metadata + archive), `--percent` sampling, fontc/archive auto-detect |
| `venv.rs` | dependency **cohorts**, pinned-dep install, the multi-Python ladder, self-healing pin relaxation |
| `rules.rs` | per-family pre-compile commands (`build_rules.json`) |
| `provenance.rs` | **M0** compiler + builder version strings recorded per family |
| `crater.rs` | `fontc_crater` comparison (per-family verdict tokens) |
| `fontspector.rs` | optional `fontspector` QA pass over green builds |
| `deb.rs` | optional `.deb` packaging of built families + `lintian` |
| `classify.rs` | failure-cause classification + actionable hints |
| `daemon.rs` | double-fork detach, SIGTERM handling, post-build linger, restart |
| `persist.rs` | atomic status / state / control / pidfile IO |
| `monitor.rs` | read-only `MonitorState` (mtime-gated) + the `Source` trait |
| `mirror.rs` | archive mirror pre-warmer (populate ahead of the builders) |
| `tui.rs` | crossterm dashboard — tabs, selection, detail overlays, the config editor |
| `web.rs` | hand-rolled `std::net` HTTP dashboard (`/`, `/api/status`, `/api/control`, …) |
| `util.rs` | `human`/`hms`, directory sizing, free-space helpers |

## Dev notes

- **Editing the web UI:** `web.rs` embeds the dashboard as one inline HTML/JS string. After changing
  it, sanity-check that brackets balance (the page must parse) and that `cargo build` is clean.
- **Two UIs, one source of truth:** prefer adding state to `model.rs` + `Orchestrator::snapshot()`
  and rendering it in *both* `tui.rs` and `web.rs`, rather than computing UI-only state in a frontend.
- **Helper scripts:** `install-cli.sh` (PATH launcher), `run-on-host.sh` (stop any daemon → back up
  resumable state → run with a live TUI), `tui_smoke.py` (a non-interactive TUI smoke check).

## See also

- [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) — the pipeline, schemas, and archive-safety invariants
- [`../docs/EXTENDING.md`](../docs/EXTENDING.md) — adding a frontend or a build backend
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — contributor workflow

---

*Written with the assistance of an AI agent (Claude Opus 4.8) under the guidance of @felipesanches.*
