# Documentation

Start with the [top-level README](../README.md) for what `gflib-build` is and how to run it. The
documents here go deeper.

## Guides

- [**ARCHITECTURE.md**](ARCHITECTURE.md) — how the pipeline, the daemon/monitor split, the on-disk
  schemas, and the archive-safety invariants fit together.
- [**EXTENDING.md**](EXTENDING.md) — how to add a new frontend (UI) or a new build backend.
- [**migration-milestones.md**](migration-milestones.md) — the M0–M7 north-star ladder toward an
  all-Rust, latest-`fontc`, zero-Python build of the whole library.
- [**SPECIFICATIONS.md**](SPECIFICATIONS.md) — the original requirements, preserved verbatim.

## Design reports

Investigations and design notes, each as Markdown + a rendered PDF (the intermediate `.html` is
git-ignored; regenerate with `md2pdf.py` → headless Chromium):

- [**build-fix-provenance**](build-fix-provenance.md) ([pdf](build-fix-provenance.pdf)) — recording
  which compiler + version built (or failed) each family.
- [**build-rules-review**](build-rules-review.md) ([pdf](build-rules-review.pdf)) — a review of every
  per-family pre-compile rule in `build_rules.json`: which are legitimate upstream steps vs.
  build-forcing workarounds.
- [**crater-source-location**](crater-source-location.md) ([pdf](crater-source-location.pdf)) —
  comparing our build status against `fontc_crater`.
- [**debian-packaging-plan**](debian-packaging-plan.md) ([pdf](debian-packaging-plan.pdf)) and
  [**deb-packaging-maintainers-guide**](deb-packaging-maintainers-guide.md)
  ([pdf](deb-packaging-maintainers-guide.pdf)) — packaging built families as `.deb`s.
- [**prebuild-investigation.md**](prebuild-investigation.md) — how the `build_rules.json` pre-compile
  steps were derived from failed-build families.
- [**cohort-map.md**](cohort-map.md) — a generated snapshot of the full-library dependency-cohort grouping.

## Tooling

- [`md2pdf.py`](md2pdf.py) — the dependency-free Markdown → HTML converter used for the report PDFs
  (`python3 md2pdf.py in.md out.html`, then render with headless Chromium `--print-to-pdf`).
