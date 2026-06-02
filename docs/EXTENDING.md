# Extending gflib-build

The build **core** (`Orchestrator`) is deliberately decoupled from any UI. You can add a
new frontend in-process, or drive an entirely separate (e.g. web) UI out-of-process from
the files the core writes. You can also add a build backend.

## Option A — a new in-process frontend

A frontend is any object with a blocking `run()` that observes the orchestrator and
returns when the run is done (or the user quits). Subclass `Frontend` and register it.

```python
class MyFrontend(Frontend):
    def run(self):
        while True:
            snap = self.orch.snapshot()      # thread-safe aggregate (see ARCHITECTURE.md)
            # ... render snap['counts'], snap['building'], snap['failures_recent'], ...
            if snap["done"]:
                self.orch.stop.set()
                break
            time.sleep(0.5)

FRONTENDS["mine"] = MyFrontend               # now selectable with --ui mine
```

Guidelines:
- **Poll `self.orch.snapshot()`** for the live aggregate; it copies state under the lock.
- To stream per-event detail, diff successive snapshots (as `PlainFrontend` does) or tail
  `events.jsonl`.
- Set `self.orch.stop` when you want workers to wind down (they finish in-flight builds).
- Read `self.orch.paused` / call `.set()`/`.clear()` to pause dispatch.
- Don't import heavy/optional libraries at module top level — import inside `run()` so the
  dependency is only required when that frontend is actually selected (this is why
  `CursesFrontend` imports `curses` lazily and falls back to `plain` if it's unavailable).

## Option B — an out-of-process / web UI (no Python coupling)

Run the build with `--ui none` (or `--ui json`) and consume the two files the core keeps
under `--build-dir`:

- **`state.json`** — poll it for the full current state (atomic replace on each write).
- **`events.jsonl`** — `tail -f` it for an append-only stream of
  `started` / `venv` / `built` / `failed` events.

A web server can watch these and push to the browser; nothing needs to link against the
harness. `--ui json` additionally prints one `snapshot()` JSON object per second to
stdout, convenient for piping into another process.

The schemas are documented in [ARCHITECTURE.md](ARCHITECTURE.md#state--events-consumable-by-any-frontend-or-external-tool).

## Option C — a build backend

Backends are selected by `Orchestrator._backend_order()` and executed by `run_builder`.
Today: `fontmake` (Python) and `fontc` (Rust, via `gftools.builder --experimental-fontc`).
To add one (e.g. a future fully-native Rust builder):

1. Extend `run_builder` to assemble that backend's command line.
2. Add the name to `_backend_order` (e.g. put it first in the `auto` chain) and to the
   `--backend` argparse `choices`.
3. The per-family `Result.backend` field and the `backends` snapshot counter then track
   it automatically — feeding the Rust-migration metric.

Because the loop already does a **fresh extraction per backend attempt**, adding a new
preferred backend that falls back to the existing ones is safe and "from scratch".

## Testing locally

- `--list` and `--cohorts-report` are read-only and instant — good smoke tests of
  discovery and cohort logic without building anything.
- `--percent 1 --only ofl/<family>` builds a single known family end-to-end.
- `--keep-work` preserves the extraction under `work/<slug>` for inspection.
- Cohort logic is unit-testable directly: `cohort_key_for(text)` is a pure function, and
  `VenvManager` can be exercised against a temp `--build-dir` with a trivial
  `--base-requirements`.

## Conventions

- Pure standard library only in the harness (it must run on a stock laptop with just
  Python 3.8+). All third-party font tooling lives in the build venv(s), invoked as
  subprocesses.
- Never write into the source repos or the archive (see the archive-safety invariants in
  ARCHITECTURE.md). New code that touches a mirror must use read-only git plumbing.
