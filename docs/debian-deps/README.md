# Archive-pure dependency packaging (the Build-Depends burn-down)

gflib-build's true build-from-source font packages `Build-Depends: gftools-builder, fontc`.
To make those — and **all** their dependencies — real Debian source packages **without vendoring**
(the chosen "archive-pure" path), package the missing crates as individual `librust-*-dev` source
packages via **debcargo**, then the two tool binaries via **dh-cargo**, reusing the ~540 ecosystem
crates Debian already ships.

This is **host work** (debcargo + sbuild; no `.deb` builds happen in the VM). These files are the plan:

| file | purpose |
|------|---------|
| `gen_manifest.py` | regenerates the burn-down from `gftools-builder3/Cargo.lock` |
| `manifest.json`   | machine-readable: each package's source/kind/deps-in-set, in build order |
| `build-order.md`  | the human-readable bottom-up debcargo/dh-cargo order |
| `verify-debian.sh`| **run on host**: lists crates.io crates missing from Debian to add to the set |

## Procedure (host)

1. `./verify-debian.sh` — confirm which crates.io crates Debian is missing; add any new ones to
   `SPECIALIST_MISSING` in `gen_manifest.py` and re-run it. (A "registry" source ≠ in Debian — the
   fontations family is crates.io-published yet absent, so it's already listed.)
2. Set up a local apt repo + an `sbuild` chroot (trixie/sid) with `debcargo`, `dh-cargo`, `cargo`.
3. Walk `build-order.md` top-to-bottom:
   - **crates.io crate** → `debcargo package <crate> <version>` → `sbuild` → publish to the local repo.
   - **git-pinned crate** → `debcargo` against a local checkout at the pinned rev, version-encode the
     commit (e.g. `0.5.0+git<YYYYMMDD>`); `sbuild`; publish.
   - **tool** (`fontc`, `gftools-builder`) → `dh $@ --buildsystem=cargo`, shipping `/usr/bin/<tool>`;
     `Build-Depends` = the `librust-*-dev` packages from the steps above + the in-Debian ones.
4. Build a font package: its `Build-Depends: debhelper-compat (= 13), gftools-builder, fontc` now
   resolve to real local packages; `sbuild` it offline.

## The M5 burn-down metric

Every `fonts-gf-*` package's `Build-Depends` is now machine-queryable. The set of **Python** tool
packages still referenced across the library is the M5 blocker list; as families move to the Rust
toolchain it shrinks. With `python_policy=off` the target set is already just these two Rust tools +
their (now archive-pure) crate graph — **0 Python**.

> Feature-gate fontspector/QA **off** when building the tools so its ~10 crates never enter the graph.
