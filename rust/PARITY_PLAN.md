# Rust port → drop-in replacement: parity plan

> **DECISION (2026-06-04): Option A — full drop-in.** Felipe chose to faithfully port the entire
> Python tool, **including the cohort venv machinery (R2)**, for a true 1:1 replacement. §4's
> recommendation toward (B)/(C) is therefore overridden; R2 is in scope in full. Execution order is
> unchanged: harness → R1 → R2 → … (R1 first because it closes the persistence gap at low risk).

Goal: make `rust/` a **drop-in replacement** for `gflib_build.py` (4674 lines). This plan inventories
every gap, grounds the effort in the actual Python subsystems, sequences the work into milestones,
and — importantly — surfaces a **strategic decision** (§4) that should be made *before* the biggest
chunk, because the largest port (cohort venvs) is the very thing the north star wants to delete.

## 0. Definition of done (acceptance criteria)

A build is "drop-in" when, on the same inputs, the Rust port matches the Python tool on **all** of:

1. **CLI surface** — every flag parses and behaves identically.
2. **Build outcomes** — ≥ the same families build; byte-identical fonts where Python is byte-identical.
3. **On-disk schema** — `state.json` (incl. `cohort_members`/`cohort_reqs`/`elapsed_so_far`),
   `timings.json`, `events.jsonl`, `migration.json`, `failure-history.jsonl` all written in the
   Python format.
4. **Insights re-surfaced** — cohorts view, timings, the 9-bucket failure taxonomy, migration breakdown.
5. **Daemon/ops semantics** — detach-by-default, auto-attach, lingering daemon, `--stop` graceful.
6. **UI fidelity** — editable config tab, section water-fill layout, status panel, live archive grid.
7. **A green parity-diff harness** (§1) proves 1–4 automatically.

Current Rust coverage: discovery (parity-verified counts), the core build pipeline (read-only
`git archive` → config → builder → collect), M0 provenance, resumable `state.json` read (verified on
the real 22h run), live control (jobs/percent/pause/retry), `--compare`, longest-first scheduling,
both UIs (rendering the shared snapshot), `--list/--attach/--stop/--reset`. **~19 cargo tests green.**

## 1. Build the parity harness FIRST  ·  effort S · risk low

Before porting features, build the *measurement* that proves drop-in-ness (very much in the M0 spirit):

- A script that runs **Python and Rust on identical inputs** (same `--only` set + copied build dirs)
  and **diffs**: per-family status, `state.json` `results`, `status.json` fields, failure taxonomy,
  built-font sha256s.
- A **monitor-parity** check: both UIs render the *same* `status.json` to equivalent output.
- Wire it into `cargo test` / a shell runner so every milestone below is verifiable, not asserted.

This is the backbone — each item is "done" only when the harness diff shrinks.

## 2. Milestones

### R1 — Persistence & insight parity (no lost data on resume/monitor) · M · low risk
*Directly resolves the "do my insights survive?" concern; lowest risk; do first.*
- Read+write `state.json` top-level `cohort_members`, `cohort_reqs`, `elapsed_so_far` (→ cumulative
  clock, not reset to 0), `saved_at`, `build_dir`.
- Populate the **cohorts view** from `cohort_members`/`cohort_reqs` (currently emitted empty).
- Read+write `timings.json` + per-family `timings`; per-op/per-phase stats in the snapshot + stats tab.
- Append `events.jsonl` (started/built/failed/venv) in Python's format.
- Write `migration.json`; richer breakdown (`fontmake_only`/`both_identical`/`both_differ`).
- Port `categorize_failure` **verbatim** (273–345: 9 buckets + actionable hints) so live
  "failures by cause" matches Python instead of the current coarser Rust classifier.
- Round-trip the extra `Res` fields (`config_used`, `fontc_ok`/`fontmake_ok`/`vs`, `retries`).
- **Accept:** monitor-parity green; resume keeps the cumulative clock + full cohort/timing/cause views.

### R2 — Cohort venv management (build-correctness core) · XL · ✅ CORE DONE
*Implemented in `src/venv.rs` + wired into the orchestrator. `cohort_key`/marker hashes are
byte-identical to the Python tool (verified), so the Rust port **reuses the existing `venvs/`** rather
than rebuilding 172 of them. Remaining polish: surface relaxations in the TUI config tab; expose
`--cohorts-report`; verify a full create-from-empty venv install end-to-end on a non-live build dir.*
Port `VenvManager` (863–1010) + `scan_cohorts` + `relax_requirements`:
- hash-keyed `cohort_key`; `venvs/<key>/` shared venvs; shared `pip-cache/`.
- per-cohort install **lock** (install once under full parallelism); `ensure_base` up front.
- `.gflib-installed` success marker → **self-healing** (rebuild a half-installed venv, never reuse).
- hermetic readiness keyed to the requirements hash; seed setuptools+wheel (Py<3.12).
- **dependency relaxation/self-heal**: parse pip `ResolutionImpossible`, auto-relax base pins we
  control (shared across cohorts), record the relaxations for the UI; classify too-deep backtracking.
- read `requirements.txt` from mirrors **read-only** (`git show`), no extraction.
- wire into `build_one` (replace the single `--build-python` with the cohort's python).
- **PIN_OVERRIDES** (from the 22h failure assessment): force `compreffor>=0.5.6` + drop the
  `fontbakery[googlefonts]` extra up front, folded into the readiness hash. Already in the Python
  tool; must be reproduced here so Rust-built venvs recover the same ~125 families.
- **Accept:** the dependency-heavy families that fail today build; parity harness shows Rust
  built-count ≈ Python on a dependency-mixed sample; venvs reused across runs (cached marker shown).

### R3 — Build-path completeness · L · ✅ MOSTLY DONE
*build_rules pre-build ✅, `--cohorts-report` ✅, `--mirror-missing`/clone (abortable+retry) ✅,
`--backend both` + sha256 compare ✅. Remaining: the streaming archive pre-warmer + the
table-tag-level compare diff (sha256-level done).*
- `build_rules.json` pre-build (`run_pre_build`): ordered shell cmds, `cwd`=work, venv bin on PATH,
  *before* the builder; non-zero exit → `pre-build` failure.
- `--backend both` + `compare_backends` (sha256 + `diff_font_tables` OT-table diff) → the `vs` column.
- `--mirror-missing` + `populate_archive` + `ensure_mirror` + clone **retry/backoff** (retryable vs
  permanent errors) + the streaming **archive pre-warmer** (idle-I/O mirror-ahead; per-repo clone lock).
- `--cohorts-report` (read-only cohort preview + `cohorts.json`).
- **Accept:** each flag matches Python on a sample; populate clones only *missing* mirrors (append-only).

### R4 — Daemon lifecycle & ops · L · ✅ CORE DONE
*Implemented in `src/daemon.rs`: double-fork `daemonize()` (before any thread is spawned),
detach-by-default for curses, a lingering daemon that idle-exits ~30 min after completion (so live
`[R]` retry works), and a SIGTERM handler for graceful `--stop`. Verified end-to-end: detach →
daemon writes pid+status, lingers, `--stop` exits it gracefully and clears the pidfile. Remaining:
reexec-wizard on `C`.*
- True detach: **double-fork** `daemonize()` (4176) — must run **before** any worker thread is spawned
  (fork-after-threads keeps only the forking thread); redirect stdio → `daemon.log`; write `daemon.pid`.
- Lingering daemon after completion (status writer + control watcher stay alive; idle-exit after 30 min).
- `--stop` graceful via a **SIGTERM handler** (flush a final status, clear the pidfile).
- `reexec` wizard on `C` (re-exec with `--wizard`); detach-by-default for interactive curses
  (`q` frees the shell, the build keeps running; re-run reattaches without setup).
- **Accept:** `q` leaves it running; re-run reattaches; `--stop` ends cleanly; survives terminal close.
- **Risk:** fork + threads + signals in Rust — fork first, minimal async-signal-safe handler.

### R5 — TUI full fidelity · L–XL · 🔶 PARTIAL
*Editable config tab (↑↓ pick, ←→ change, applied live via control.json) ✅, completion/stopped
banner ✅. Remaining: section water-fill layout, the live archive multi-column grid, stable
selection (track item not row index), detail-overlay parity.*
Port the parts of `CursesFrontend` (2637–3861, ~1225 lines) the Rust TUI doesn't have yet:
- **editable config tab** (fields, text cursor, ±step, choice cycle, checkbox→conditional reveal);
  live apply via `control.json` (✓ apply changes); first-run ▶ Start / Cancel.
- **section navigation** (←/→ focus, ↑/↓ items) + **water-fill layout** planner (fills vertical
  space; live resize reflow) — port the algorithm *and* its 330k-case sweep test.
- always-on **status panel** with per-item explanations (incl. *why* an archive repo couldn't mirror).
- live **archive multi-column grid** (cloning-now/recent/queued) + unreachable list.
- completion/stopped **banner**; **stable selection** (track item, not row index); detail-overlay parity.
- **Accept:** Rust equivalents of `tests/pty_*.py` pass.

### R6 — Live-config parity · M · ✅ DONE
*Raising `--percent` live enqueues the newly-included families (all_families kept) ✅; jobs/percent/
backend/compare/pause all live ✅; prior-run reconcile is implicit (only the current worklist is
materialised) ✅.*
- `apply_live`: raising **percent** enqueues newly-included families (fetch/cohort/build live);
  backend/compare/timeout/populate live; surface dependency relaxations; `control_log` parity.
- reconcile a prior higher-percent run's leftover `queued`/`building` → `skipped (not selected)`.
- **Accept:** raising percent on a running Rust build pulls in more families; counts stay coherent.

## 3. Sequencing & rough effort

`Harness(S) → R1(M) → R2(XL) → R3(L) → R4(L) → R5(L–XL) → R6(M)`. R5 can overlap R2–R4. R1 first:
it's the user's stated concern, low risk, and makes everything measurable. Realistically several
focused weeks solo, or a much shorter orchestrated multi-agent push (one agent per milestone module
against the parity harness).

## 4. ⚠️ Strategic decision to make BEFORE R2

The north star (see `docs/migration-milestones.md`, M5/M7) is an **all-fontc, zero-Python** build.
The cohort venv system (R2) exists **solely to manage Python build dependencies** (fontmake / gftools
/ setuptools). It is the **biggest, riskiest** port — and it is **exactly the machinery the north star
wants to delete.** Faithfully reproducing it in Rust optimizes a path we intend to retire. Choose:

- **(A) Full drop-in incl. the Python venv machinery** — port R2 faithfully. True 1:1 today, but
  carries forward Python-build complexity we mean to remove.
- **(B) Drop-in for everything *except* the Python-build path** — Rust does discovery / monitoring /
  persistence / **fontc + builder3** builds natively (no venvs), and **shells out to the Python tool**
  (or `gftools.builder`) only for families still needing the Python path, until builder3 covers them.
  Far less Rust venv code; aligned with the north star.
- **(C) Hybrid** — port a *minimal* venv path (base cohort + `.gflib-installed` reuse, no relaxation /
  self-heal) to unblock the easy dependency families; treat the long tail as Python-path.

**Recommendation: (B) or (C).** Spending XL effort to port the Python venv system into Rust optimizes
a path we're trying to eliminate. Confirm direction with Felipe before starting R2.

## 5. Testing & verification
cargo unit tests (per module) · pty TUI tests (port `tests/pty_*.py`) · the **parity-diff harness**
(golden Python-vs-Rust run) · monitor-parity check · archive-safety invariant tests (read-only
`git archive`; the archive is never written).

## 6. Risk register
- **fork + threads + signals** (R4): fork before spawning threads; minimal async-signal-safe handler.
- **venv concurrency / self-heal** (R2): per-cohort locks; hash-keyed readiness; the relaxation set.
- **TUI water-fill layout** (R5): subtle — port the planner *and* its sweep test.
- **virtiofs I/O slowness** (observed): keep ≤2–3 parallel build agents; drop caches during heavy I/O.

---
*Plan authored by an AI agent (Claude Opus 4.8) under the guidance of @felipesanches.*
