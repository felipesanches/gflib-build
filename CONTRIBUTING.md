# Contributing to gflib-build

Thanks for taking a look! `gflib-build` is an early-stage prototype, so the most useful
contributions right now are trying it on a small sample, reporting what breaks, and small focused
improvements. This guide gets you oriented.

## Get set up

```sh
cd rust
cargo build --release        # the tool: target/release/gflib-build
cargo test                   # run the unit tests — keep these green
cargo run -- --help          # the full CLI surface
```

Then try it without touching real builds:

```sh
cargo run -- --demo --ui web     # replays a recorded session live at http://localhost:8765
```

To exercise a real (tiny) build you need `git` and `python3` on your `PATH` and a worklist source —
a `google/fonts` clone or a repo archive of bare mirrors. Always start small:

```sh
cargo run -- --google-fonts /path/to/google/fonts --only ofl/abel
```

## Find your way around

- The code lives in [`rust/src/`](rust/src/) — see the **module map** in
  [`rust/README.md`](rust/README.md) for what each file does.
- The design and the daemon/monitor split are in
  [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
- Adding a UI or a build backend is in [`docs/EXTENDING.md`](docs/EXTENDING.md).
- What we're building toward (and why the tool tolerates Python today) is the M0–M7 ladder in
  [`docs/migration-milestones.md`](docs/migration-milestones.md) — a good place to find work that
  matters.

## Conventions

- **Match the surrounding code.** Mirror the existing naming, comment density, and idioms in the file
  you're editing rather than introducing a new style.
- **One source of truth for UI state.** Prefer adding fields to `model.rs` + `Orchestrator::snapshot()`
  and rendering them in **both** `tui.rs` and `web.rs`, rather than computing display-only state
  inside one frontend.
- **Keep dependencies minimal.** The tool builds with only `serde`, `serde_json`, and `crossterm`.
  Please discuss before adding a crate.
- **Preserve the archive-safety invariants.** Sources are read read-only (`git archive` from bare
  mirrors); upstream repos are never modified or deleted; all output goes under the build dir.
- **Tests stay green.** Add a unit test when you touch parsing, schema, or cohort/venv logic; run
  `cargo test` before sending a change.
- **Commit in small, focused steps** with clear messages.

## A note on `build_rules.json`

The per-family pre-compile rules are sensitive: some edit the upstream source and can change the
shipped font. Before adding or changing one, read
[`docs/build-rules-review.pdf`](docs/build-rules-review.pdf) — a rule should ideally reproduce a step
the upstream project itself performs, not force a build by patching the design.

## License

By contributing you agree that your contributions are licensed under the project's
[Apache License 2.0](LICENSE).
