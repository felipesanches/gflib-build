# Retrigger notes — venv-failure fix (run externally, on the host)

Per the hard rule, the actual build/retrigger is run by you on the host, never inside the VM.
Everything below is left ready.

## What was fixed (committed)
Root cause of the ~332 "venv" failures: the pinned baseline is a **pre-Python-3.13 freeze**, so
many packages had no cp313 wheel and built from sdist, where they hit **setuptools 82's removal of
`pkg_resources`** (build- and runtime). Fixes (`rust/src/venv.rs`, `classify.rs`):

- **relax-to-wheel** — when a pinned sdist won't build, the self-heal relaxes that pin so pip takes a
  cp313 **wheel** instead (openstep-plist 0.3.1→0.5.2, lxml→6.x, numpy/markupsafe/cffi/skia-pathops…).
  No compiler needed → **`python3-dev` is NOT required.**
- **setuptools<81** — seeded into every venv and applied to pip's isolated build envs via
  `PIP_CONSTRAINT`. The toolchain (gftools/fontmake + deps) imports `pkg_resources`, which setuptools
  82 removed; setuptools itself says "pin to <81". Genuine toolchain requirement, not a workaround.
- The VM-only virtiofs/ENFILE handling was **reverted** from the tool's source (kept external).

Validated end-to-end in-VM on one family (`ofl/alata`): venv installed with setuptools 80.10.2
(pkg_resources OK) and the font BUILT.

## State left ready
- `gflib-data/build/state.json`: **605 built** (recovered from on-disk fonts), 807 queued, 11 failed.
  (Note: an in-VM test accidentally clobbered the prior bookkeeping; recovered from `out/`. The
  clobbered copy is saved as `state.json.clobbered-by-test-*`.)
- `gflib-data/gflib-build.config`: `jobs` restored to 10.
- No daemon/watchdog running; stale `daemon.pid`/`control.json` cleared.

## To run on the host
```bash
cd <repo>/rust && cargo build --release      # build the fixed binary
# IMPORTANT: the existing venvs under gflib-data/build/venvs/ were created with setuptools 82 and are
# broken for pkg_resources. For a clean result, delete them so every cohort rebuilds with setuptools<81:
rm -rf <repo>/gflib-data/build/venvs/*
# then run (resumes from state.json, auto-retries the venv failures, rebuilds cohorts with <81):
<repo>/rust/target/release/gflib-build --data-dir <repo>/gflib-data
```
The failed-cohort venvs rebuild with setuptools<81 automatically; deleting `venvs/*` also forces the
setuptools-82 venvs of any retried (non-venv) failures to be rebuilt cleanly. `build_debs` is on, so
built families are auto-packaged into `gflib-data/build/packaging/`.

> Note: the venv **policy epoch** committed in `83b14fe` bumps the cohort marker hash, so the
> setuptools-82 venvs are now rebuilt automatically on the next run even without `rm -rf venvs/*`.
> Deleting them is still the cleanest way to guarantee a from-scratch rebuild of every cohort.

## Retriggering affected families after a fix (the general mechanism)

After applying any build fix, rebuild exactly the affected families without redoing the whole library:

- A named set (e.g. the families whose `config.yaml` you just changed):
  ```bash
  gflib-build --data-dir <repo>/gflib-data --retrigger ofl/amaranth,ofl/amaticsc,ofl/asimovian
  ```
- By failure cause (re-attempt one bucket): `--retry-category "<cause>"` (causes shown in the failures tab)
- All failures: `--retry-failed`

`--retrigger` / `--retrigger-crater` force a rebuild regardless of prior built/failed status; the
self-heal auto-retry set already re-attempts venv/clone/mirror failures on every plain run.

## fontc_crater comparison + the gold families

gflib-build loads fontc_crater's latest per-family verdict and shows it in the **`crater` tab** (and as
a token on every built/failed/queued row). The headline bucket is **we build · fontc can't** — families
WE compile that fontc_crater's fontc cannot. Those build fixes are the most valuable to the fontc team,
and the very same `config.yaml` / build rules that unblock our Debian packaging unblock crater too
(both resolve `sources` against the repo root identically).

Rebuild exactly the families fontc_crater's fontc fails on (to discover which we can fix), then read
the crater tab's GOLD list:
```bash
gflib-build --data-dir <repo>/gflib-data --retrigger-crater fontc-failed
```

Data source: the comparison reads `fontc_crater_targets.json` (complete per-target status, written by
gfonts_agents' `fetch_crater_analysis.py`). Until you next refresh the dashboard it falls back to the
diff-only `fontc_crater_analysis.json` (the crater tab flags this as **PARTIAL** — the fontc/both-failed
split is absent in the fallback). Override the source with `--crater <path>`, disable with `--no-crater`.
