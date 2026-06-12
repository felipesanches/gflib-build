# Architecture

> The only implementation is the **Rust** code in [`../rust/`](../rust/). The earlier single-file
> Python program has been removed (it lives in git history). The on-disk JSON schemas it defined are
> kept intact, so a status/state/control file written by either side is still mutually readable.

`gflib-build` orchestrates building every buildable family in the Google Fonts library from pristine
archived sources, and renders progress through one of several interchangeable UIs. The actual font
compile is delegated to a separate interpreter/venv (`gftools.builder` + `fontmake`), to the `fontc`
binary, or to a Rust-native `builder3` — so the engine never imports a font toolchain itself.

## High-level shape: daemon + monitor, joined by files

A build is owned by a single background **daemon** — the `Orchestrator` in `build.rs`. It writes an
atomic `status.json` snapshot about once a second and reads live commands from `control.json`. Every
UI is a thin **monitor**: it renders a `Snapshot` and posts back a `ControlSet`. The terminal UI, the
web UI, and any external tool therefore observe the same state through the same files, and exactly one
daemon owns a build directory at a time.

The seam between them is the `Source` trait (`monitor.rs`):

```rust
trait Source {
    fn snapshot(&self) -> Snapshot;      // what to render
    fn build_dir(&self) -> PathBuf;
    fn is_live(&self) -> bool;           // true: we own the build; false: read-only monitor
    fn control(&self, set: &ControlSet); // live → apply in-process; monitor → write control.json
    fn request_stop(&self) {}            // live only
}
```

Two types implement it: the live `Orchestrator` (a real build) and `MonitorState` (attached to
someone else's daemon, reading `status.json`). A third, `SetupState`, is a static source for the
first-run config wizard. Because both UIs render against `Source`, the dashboard is identical whether
you launched the build or are just watching it.

`MonitorState` re-parses `status.json` only when its mtime changes and throttles its filesystem stat
to a few times a second, so the dashboard stays snappy even over a networked filesystem.

## Module map (`rust/src/`)

For a one-line responsibility per module see [`../rust/README.md`](../rust/README.md). The
load-bearing modules for this document:

| Module | Role in this design |
|--------|---------------------|
| `build.rs` | the `Orchestrator`: worker pool, per-family pipeline, live-control application, the archive pre-warmer, the package worker |
| `daemon.rs` | double-fork detach, SIGTERM/`--stop`, post-build linger, restart/respawn |
| `monitor.rs` | the `Source` trait, `MonitorState`, `SetupState` |
| `persist.rs` | atomic status/state/control IO + `daemon.pid` liveness |
| `model.rs` | the serde schema: `Snapshot`, `Res`, `StateFile`, `Control`/`ControlSet` |
| `venv.rs` | dependency **cohorts**, the multi-Python ladder, self-healing pin relaxation |
| `discover.rs` | worklist discovery (metadata + archive), `--percent` sampling, fontc/archive auto-detect |
| `rules.rs` · `classify.rs` · `provenance.rs` | pre-build commands · failure taxonomy · M0 version strings |
| `crater.rs` · `fontspector.rs` · `deb.rs` | fontc_crater comparison · QA pass · `.deb` packaging |
| `mirror.rs` · `config.rs` · `util.rs` | abortable mirror cloning · CLI/config-file · sizing helpers |
| `tui.rs` · `web.rs` | crossterm dashboard · hand-rolled `std::net` HTTP dashboard |

## The streaming build pipeline

`Orchestrator::new()` builds the worklist (from a google/fonts clone's METADATA, or from the bare
mirrors directly with `--source archive`), reconciles it against any prior `state.json`, applies
`--percent` sampling, and enqueues the families that still need work. `Orchestrator::start()` then
spawns, with **no global barriers**, all of:

- the **worker pool** (`--jobs` threads running `worker_loop`), each of which pulls a slug and runs
  the per-family pipeline below;
- the **archive pre-warmer** (`spawn_archive_prewarmer`, only with `--mirror-missing`): a modest pool
  that proactively `git clone --mirror`s every worklist repo whose mirror is absent, so the archive
  reaches 100% regardless of build pace. It shares a per-repo clone lock (`clone_locks`) with the
  workers, so no repo is ever cloned twice;
- the **status writer** (`spawn_status_writer`): writes `status.json` every ~1 s and the derived
  `migration.json` / `timings.json` every ~10 s;
- the **size thread**: samples build-dir size, free space, archive size + repo count, and the
  on-disk cohort-venv set;
- the **control watcher** and **config watcher** (see below);
- optionally the **QA pool** (`--fontspector`) and the **package worker** (`.deb` building).

Nothing waits on a "mirror-all, then scan-all, then build" barrier: each worker mirrors-on-demand,
assigns its cohort, and compiles the moment that family's repo is available. Cohorts are evaluated
per-repo as repos land.

### Per-family build pipeline (`Orchestrator::build_one`)

1. **locate the mirror** — `mirror_path()` maps the repo URL to its bare mirror under the archive.
   With `--mirror-missing` an absent repo is cloned `--mirror` (append-only, one clone per repo under
   `clone_locks`); without it, an absent mirror fails the family cleanly.
2. **cohort venv** (with `--manage-venvs`) — read the family's `requirements.txt` **read-only** from
   the mirror (`read_requirements_from_mirror`, a `git show`, no checkout), then obtain the shared
   cohort interpreter via `VenvManager::get_python`. Without `--manage-venvs`, every family builds
   with the single `--build-python`.
3. **chain attempts** — `attempt_chain()` yields the (orchestrator, compiler) ladder (see
   *Build backends* below): builder3+fontc → builder2+fontc → builder2+fontmake for `auto`. For
   each attempt:
   - **`extract_tree`** streams the committed tree at the pinned commit into `work/<slug>` via
     `git archive | tar -x` — the only way sources are ever read;
   - any registered **pre-build commands** (`rules::run_pre_build`) run in the extracted tree;
   - **`preclean_outputs`** wipes committed build artifacts (`fonts/`, `*_ufo/`, `build*.ninja`, …)
     so everything is regenerated;
   - **`resolve_config`** picks the gftools-builder config (see below);
   - **`run_builder`** compiles.
4. **`collect_outputs`** copies freshly-built (`mtime`-gated) `.ttf`/`.otf` whose names match the
   family's shipped binaries into `out/<slug>`, recording bytes and any missing shipped files. It
   also scans the stray `../fonts` dir an override config may write to, without ever mis-attributing
   another concurrently-building family's fonts.
5. **compare** (with `--compare`, metadata mode) — sha256 built vs shipped → `identical` / `differ` /
   `missing`. This, plus the recorded backend, is the Rust-migration signal.
6. **cleanup** — a guard always removes `work/<slug>` (unless `--keep-work`). Nothing leaks, and
   nothing is ever left inside a repo.

On success, `build_one` records the result and the M0 provenance; on failure, `fail()` records the
cause (classified by `classify.rs`), appends a durable line to `failure-history.jsonl`, and archives
the failing log under `logs/failed/`. Provenance is recorded on **both** success and failure.

`--backend both` branches into `build_both`, which builds fontc and fontmake into separate output
dirs and compares them — the fontc_crater-style equivalence check.

### Config resolution (`resolve_config`)

`gftools.builder` `chdir`s to the config file's parent and resolves `sources:` relative to there, so:

1. a **google/fonts override** (`<google-fonts>/<slug>/config.yaml`) is staged into the extraction
   **root** as `__gflib_override_config.yaml`, because override configs use repo-root-relative paths;
2. an **in-repo `config_yaml`** (from METADATA) is used in place — its `sources:` are already
   relative to its own directory;
3. otherwise an **auto-discovered** `sources/config.yaml` (etc.) is tried.

### Build backends — the attempt chain (`attempt_chain`, `run_builder`)

Each family runs an **(orchestrator, compiler) attempt chain**, built by the pure
`attempt_chain()` from the backend setting and which tools resolved. For `--backend auto` (the
default) it is:

1. **`builder3` + fontc** — the Rust-native `gftools-builder3` binary, invoked directly (zero
   Python; it embeds fontc as a library, so it needs no fontc binary and can never run fontmake);
2. **`builder2` + fontc** — `python -m gftools.builder <config> --experimental-fontc <bin>`;
3. **`builder2` + fontmake** — plain `python -m gftools.builder <config>`.

Every pair is the graceful fallback for the one before it; a family is counted toward the M5
(Python-free) milestone only when attempt 1 succeeded. `--orchestrator builder3|builder2` forces
one orchestrator (builder3 = an explicit no-Python-fallback run); `--backend both` compares the
two compilers under builder2 on both sides, isolating the compiler axis. `Res.builder` /
`builder_version` record the attempt that actually ran, per family, success or failure.

builder2 children run with `SOURCE_DATE_EPOCH=0` and the interpreter's `bin/` prepended to `PATH`
(gftools.builder shells out to `fontmake`/`ninja`/`ttfautohint` by name). Each child runs in its
**own process group** so a freeze/kill reaches the whole `python → fontmake → ninja` tree.

### Automatic upgrades (kept successes are never spent)

At reconcile time, a kept success **below the top rung** (fontmake, or fontc under builder2) is
re-queued as an `upgrade` — automatically, **once per toolchain signature** (the pins + the
orchestrator preference, stamped on every completed attempt as `Res.upgrade_attempted`), and
always **after** all new/retry work. An upgrade attempts only the rungs *strictly better* than the
recorded one. Before it runs, the family's current output fonts are parked under
`<build-dir>/variants/<slug>/<builder>-<backend>/`:

- **declined** (no better rung succeeded): the prior result is restored verbatim — record *and*
  binaries — and the family never appears as failed; the attempt lives in the family log and an
  `upgrade_declined` event.
- **succeeded**: the new result becomes canonical in `out/<slug>/`, and the superseded rung's
  binaries stay under `variants/` — every compiler's successful output is kept so the binaries
  can be compared later (the M3 axis).

`--no-auto-upgrade` (or the config-tab toggle) disables the pass; bumping a pin re-arms it.

### The zero-setup toolchain (`toolchain.rs`)

`fontc` and `gftools-builder3` are **guaranteed available with no user setup**. Neither can be a
Cargo dependency (fontc is binary-only; builder3 has git dependencies and is unpublishable), so
the tool provisions **pinned releases** itself: resolution per tool is explicit flag → the
provisioned pin under `<data-dir>/tools/<name>-<pin>/` → `cargo install` the pin (fontc from
crates.io, builder3 via `--git --rev --locked`) → a detected binary (PATH / sibling checkouts) as
the last resort, so a stale local build never silently shadows the pin. A resolver thread fills a
ready-gate at orchestrator start; workers wait on it (`Res.note = "waiting for toolchain"`), the
provisioning shows up as pipeline tasks, and a tool that fails to provision is simply marked
unavailable — the attempt chain degrades past it and the run continues. Pins are consts in
`toolchain.rs`; bumping them re-provisions into a new version-keyed directory on the next run.

## Live config (the control channel)

A monitor's config tab edits live-applicable settings and writes them to `control.json` via
`persist::write_control`, a `{seq, set}` document that bumps `seq` on every change. The daemon's
control watcher (`spawn_control_watcher`) polls the file and calls `apply_live(set)` only when `seq`
increases. `apply_live` clamps and applies:

- **jobs** → updates the live target and spawns more workers (`ensure_workers`);
- **percent ↑** → re-samples the full family list and enqueues the newly-included families, so the
  build keeps going;
- **paused** → freezes/thaws every in-flight build (see the regulator);
- **backend / compare / build_debs** → each subsequent build reads the new setting;
- **retry / retry_all / retry_overrides / repackage_all / restart** → one-shot actions.

The **config watcher** (`spawn_config_watcher`) is the hands-free analogue of the "retry config-fixed"
button: when a FAILED family's override-config signature changes (we wrote or edited a fix), it
re-queues that family automatically. It fires only on an actual signature change, so a still-failing
build never loops.

## Detach-by-default & auto-attach (`daemon.rs`)

A fresh interactive build **detaches by default**: `daemonize()` double-forks into a background
daemon (redirecting stdio to `daemon.log`, writing `daemon.pid`) and the foreground process attaches
a read-only monitor. `daemonize()` must run **before** any worker thread is spawned, because `fork()`
keeps only the calling thread in the child. Quitting the monitor frees the shell while the build keeps
running.

Re-running the program detects the live daemon via `read_daemon_pid()` (which probes liveness with
`kill -0`, so a stale pidfile reads as gone) and **auto-attaches the monitor**, skipping the wizard.
`--stop` sends SIGTERM for a graceful shutdown. After completion the daemon **lingers** (~30 min by
default) so a live retry or control still works; new work resets the linger timer. A UI "Restart"
routes through the same graceful path and then `respawn_if_requested` re-launches a fresh daemon.

## Dependency cohorts & self-healing installs (`venv.rs`)

Families with identical requirements share one venv, keyed by a content hash:
`cohort_key_for(requirements)` is `"base"` for empty requirements, else `"c-" + sha1[..12]` of the
normalized text (comments/whitespace stripped, lines sorted, `-r` includes inlined, QA-only tools
filtered out). One venv per cohort lives under `<build-dir>/venvs/<key>/`, created lazily under a
per-cohort lock with a shared `pip-cache`. Each venv carries a `.gflib-installed` marker (a hash of
the requirements it was built for) so a stale/half-installed venv is rebuilt, never reused. The hashes
use coreutils `sha1sum`/`sha256sum`, so cohort keys and markers are byte-identical across runs.

The **multi-Python ladder** (`--pythons`): when several interpreters are configured, the default rung
keeps the bare cohort key (so existing venvs are reused unchanged) and each older rung appends a
`-py<tag>` suffix. A rung that can't install the **exact** pinned requirements falls through to the
next older rung **without relaxing** — only the last rung relaxes — so the tool prefers "faithful pins
on an older Python" over "relaxed pins on a newer one". A commit-date heuristic skips rungs whose
interpreter is too new for the freeze era.

The **self-healing install** runs `pip install` against an assembled `effective-requirements.txt`
(base toolchain minus any pin the family overrides, plus the family's own pins). On failure the log is
parsed for pins pip can't satisfy, ResolutionImpossible conflicts, and sdists that won't build here;
those are relaxed (or wheel-forced) and the install is retried, up to a bounded number of rounds.
Globally-bad base pins are cached and shared by later cohorts. Valid pins are never touched, so
reproducibility holds for everything that resolves. Each relaxation is surfaced in the snapshot's
`dep_relaxations` (shown in the config tab).

## Concurrency & the job regulator

- A shared `VecDeque` of slugs (`Shared::queue`) feeds the worker pool; each build is an isolated
  **subprocess**, so threads suffice and `gftools.builder`'s global `chdir` can't corrupt a sibling
  build.
- Mutable orchestrator state lives in one `Shared` struct behind a single `Mutex`; `snapshot()` takes
  a consistent copy under it. A `Condvar` wakes parked workers when work or capacity changes.
- **Per-build CPU budgets.** Each of the `jobs` workers confines its build to ~`cpus/jobs` CPUs:
  a `taskset` CPU slice (disjoint across workers; inherited by every descendant, so ninja's
  cpus+2 edges or a fork-happy Python can't escape it), `RAYON_NUM_THREADS` for fontc, and
  builder3's own `--jobs`. Venv creation is additionally throttled to a few concurrent
  `pip install`s with capped sdist-build parallelism — N workers × N uncapped numpy builds was
  a real triple-digit load average in the field. `--no-cpu-slices` lifts the taskset confinement.
- The **job regulator** keeps exactly `jobs` builds *actively* compiling. Lowering `jobs` below the
  number of running builds **freezes the newest excess with SIGSTOP** rather than killing it; as
  builds finish, freed slots **thaw the oldest frozen build first** (drain in-progress before starting
  new). A global pause freezes everything. Frozen time does not count toward a build's timeout.
- Termination: a worker idles when the queue is empty; the daemon's `done` is reached when nothing is
  queued, nothing is building, no worker is in flight, and (when enabled) the QA and packaging
  backlogs have drained. `request_stop` thaws then SIGKILLs any in-flight builder groups so nothing
  orphans when the daemon exits.

## State, events & schema (`persist.rs`, `model.rs`)

All persistence lives under `--build-dir`, written atomically (temp + rename) so a reader never sees a
torn file:

- **`status.json`** — the live `Snapshot` (counts, backends, in-flight builds, cohorts, archive view,
  fail categories, M0 tooling/builder versions, QA + crater + packaging views, …), rewritten ~1 s.
- **`state.json`** — the full resumable `StateFile`: per-family `Res` results, the cohort map, and the
  cumulative elapsed clock. Written after every terminal transition and at shutdown.
- **`failure-history.jsonl`** — append-only durable record of how families broke (never erased by a
  later success).
- **`events.jsonl`** — append-only `{t, type, slug, …}` stream (`started` / `venv` / `built` /
  `failed` / `archived`) that external tools can tail.
- **`control.json`** — the live-control channel a monitor bumps and the daemon applies.
- **`daemon.pid`** — the detached daemon's PID, for attach/stop.
- **`migration.json`** / **`timings.json`** — derived reports refreshed periodically.

Every schema struct is `#[serde(default)]`-friendly, so a partial or older file still deserializes.

## Archive-safety invariants (enforced by construction)

These are non-negotiable and hold regardless of UI or backend:

- The bare mirrors are read with **`git archive` / `git show` / `rev-parse` / `cat-file` only**. The
  sole writes git ever does to the archive are `clone --mirror` (new repos, only under
  `--mirror-missing`) and `remote update` (refs only; never removes commits). **No checkout, no gc,
  no delete.**
- **Every byte the tool produces lives under `--build-dir`.** Source repos and the archive are never
  written by the build.
- Each build starts from a **fresh extraction** with committed outputs pre-cleaned, so a prior
  attempt can never contaminate the next.
- Per-family `work/<slug>` is always cleaned up (a guard runs even on panic/early-return), so nothing
  leaks and nothing is left inside any repo.

---

*Written with the assistance of an AI agent (Claude Opus 4.8) under the guidance of @felipesanches.*
