# Archive-pure dependency packaging (the Build-Depends burn-down)

gflib-build's true build-from-source font packages `Build-Depends: gftools-builder, fontc`.
To make those — and **all** their dependencies — real Debian source packages **without vendoring**
(the chosen "archive-pure" path), package the missing crates as individual `librust-*-dev` source
packages via **debcargo**, then the two tool binaries via **dh-cargo**, reusing the ~540 ecosystem
crates Debian already ships.

## gflib-build does this itself

```sh
gflib-build --package-deb-deps          # run on the HOST (needs debcargo + sbuild/dpkg-buildpackage)
```

`--package-deb-deps` (module `src/deb_deps.rs`, **pure Rust — no Python**):

1. Computes the burn-down from `gftools-builder3/Cargo.lock`: the git-pinned font crates + the
   crates.io crates Debian lacks (incl. the fontations family) + the 2 tool binaries, in
   **topological (leaves-first)** order — reusing every crate Debian already provides.
2. Writes `<build_dir>/deb-deps/manifest.json` (the plan).
3. **Drives it**: per package, `debcargo` (crates.io) / `debcargo` against the vendored source (git) /
   `dh-cargo` (tools) → `dpkg-buildpackage` or `sbuild` → publish to the local apt repo
   `<build_dir>/deb-deps/apt/` (+ `dpkg-scanpackages` index). Idempotent (skips already-built),
   continue-on-failure, per-package results in `<build_dir>/deb-deps/results.json`.
4. When `debcargo`/the build front-end are absent (e.g. in the build VM) it **dry-runs**: it prints the
   exact commands per package so you can review/tune them before running on the host.

Execution is host-only (no `.deb`/sbuild builds happen in the build VM). The planning and the per-package
command construction are unit-tested in-tree; the exec recipes are deliberately small + commented for
host tuning (esp. the git-crate `debcargo` flags and the sbuild chroot).

## verify-debian.sh

`verify-debian.sh` (host helper) lists the crates.io crates in the lock that Debian is **missing**, so
any beyond the built-in set can be added to `SPECIALIST_MISSING` in `src/deb_deps.rs`.

## The M5 burn-down metric

Every `fonts-gf-*` package's `Build-Depends` is machine-queryable. With `python_policy=off` the target
set is just `gftools-builder` + `fontc` + their (now archive-pure) crate graph — **0 Python**. As crates
move from vendored/debcargo to proper `librust-*-dev`, the burn-down is visible in `results.json`.

> The tool packages are built with fontspector/QA feature-gated **off**, so its ~10 crates never enter
> the build-deps graph.
