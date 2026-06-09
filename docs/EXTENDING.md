# Extending gflib-build

The build **core** (the `Orchestrator` in `build.rs`) is deliberately decoupled from any UI. You can
add a new frontend in-process, drive an entirely separate UI out-of-process from the files the core
writes, or add a build backend.

The seam is the **`Source` trait** (`rust/src/monitor.rs`):

```rust
pub trait Source: Send + Sync {
    fn snapshot(&self) -> Snapshot;          // the live aggregate state
    fn build_dir(&self) -> PathBuf;
    fn is_live(&self) -> bool;               // true for the live orchestrator, false for a read-only monitor
    fn control(&self, set: &ControlSet);     // apply a live command (jobs / percent / pause / retry / …)
    fn request_stop(&self) {}                // stop the build (live only); default no-op
}
```

It is implemented by **both** the live `Orchestrator` (a build you own) and the read-only
`MonitorState` (a build someone else's daemon owns), so anything written against `Source` works in
both cases — you can render your own build *or* monitor a running one with the same code.

## Option A — a new in-process frontend

A frontend is just code that takes an `Arc<dyn Source>`, renders `source.snapshot()` in a loop, and
calls `source.control(&set)` to send live commands. Use the existing frontends as references:

- `web.rs` — `pub fn run(source: Arc<dyn Source>, port: u16)` serves a browser dashboard.
- `tui.rs` — the crossterm dashboard loop.

To wire a new `--ui myui` in:

1. Add `"myui"` to the `ui` choices documented in `config.rs` (the `--ui` parser accepts any string;
   keep the doc comment and `pick_frontend` in sync).
2. Add a match arm in `main.rs`'s frontend dispatch (the `match pick_frontend(&cfg.ui)` block) that
   constructs your frontend with the `Arc<dyn Source>`.

Render `snapshot()` fields (`counts`, `building`, `failures_recent`, `queued_list`, …); don't compute
display state the core could own — prefer adding it to `model.rs` + `Orchestrator::snapshot()` so
every frontend benefits.

## Option B — an out-of-process / web UI (no Rust coupling)

Run the build with `--ui none` and consume the files the daemon keeps under `--build-dir`, or talk to
the built-in web server:

- **`status.json`** — the full `Snapshot`, rewritten atomically ~once a second. Poll it.
- **`control.json`** — write a `ControlSet` here (the daemon polls and applies it). Unset keys are
  omitted, so partial updates are fine.
- Or, against `--ui web`: `GET /api/status` returns the snapshot and `POST /api/control` accepts a
  `ControlSet` — the exact channel the built-in UIs use.

The schema is the serde types in `model.rs` (`Snapshot`, `ControlSet`). Nothing needs to link against
the tool.

## Option C — a build backend

Backends are run by the `Orchestrator` in `build.rs` (`run_backend_into` and the backend-ordering
logic). Today: `fontc` (Rust), `fontmake` (Python), `both` (build each and compare), and the Rust
`builder3` orchestrator, selected by `--backend {auto,fontc,fontmake,both}`. To add one (e.g. a new
fully-native Rust builder):

1. Assemble its command line in `build.rs` (alongside the existing backends).
2. Put it in the backend order — e.g. first in the `auto` chain so it's tried before the fallbacks.
3. Add the name to the `--backend` field comment in `config.rs` **and** to the `--backend` line in
   the help text (`print_help` in `main.rs`), so the documented choices stay in sync.

The per-family `backend` field and the `backends` snapshot counter then track it automatically —
that record is the **Rust-migration metric** (M2–M4). Because the loop does a **fresh extraction per
backend attempt**, adding a preferred backend that falls back to the existing ones is safe and
"from scratch".

## Testing locally

- `--list` and `--cohorts-report` are read-only and instant — good smoke tests of discovery and
  cohort logic without building anything.
- `--demo` (a.k.a. `--dry-run`/`--mock`) replays a recorded session live — exercises the UIs with no
  real clone/venv/compile.
- `--only ofl/<family>` builds a single known family end-to-end.
- `cargo test` covers the unit-level logic (serde round-trips, parsers, cohort/venv behavior). Add a
  test when you touch any of those.

## Conventions

- **Minimal dependencies** — the tool builds with only `serde`, `serde_json`, and `crossterm`.
- **Never write into the source repos or the archive** — the archive-safety invariants
  (read-only `git archive`, append-only archive, all output under the build dir) are non-negotiable;
  new code that touches a mirror must use read-only git plumbing.

See [`../CONTRIBUTING.md`](../CONTRIBUTING.md) for the broader contributor workflow.
