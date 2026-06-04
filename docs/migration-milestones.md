# Python → Rust migration milestones

**North star.** Build the **entire Google Fonts library** reproducibly and correctly with the
**latest `fontc`** and **zero Python in the build pipeline** (no gftools-builder2 / fontmake, no
Python pre-build scripts, no Python-only deps) — a **fully-Rust** pipeline.

**Why `gflib-build` tolerates fontmake / old fontc today.** We are not there yet. The tool
deliberately uses whatever compiler works *right now* — **as long as it records the compiler and
its exact version for every build attempt, success or failure.** That recorded data is what lets us
*morph gradually* toward the all-Rust future and measure every intermediate step. **Every feature
should be designed with these milestones in mind** (this is the concrete meaning of spec 12).

## The build path today, and `gftools-builder3`

A subtlety that the ladder below depends on: **today, every build — fontc included — runs through
the Python orchestrator `gftools.builder` (the current "builder2").** `gflib-build` shells out to:

```
python -m gftools.builder <config.yaml>                       # fontmake path
python -m gftools.builder <config.yaml> --experimental-fontc <fontc-bin>   # fontc path
```

So even when **fontc** does the compilation, `gftools.builder` (Python) is still the *orchestrator*
— it reads the config, runs the compiler, then does instancing, fixing, autohinting, and packaging,
much of which is still Python (and shells out to `fontmake`/`gftools`/`ttfautohint`/`ninja` by name).
**That means an L4 family — "fontc builds equivalent output" — is still NOT Python-free.** Swapping
the *compiler* to fontc is necessary but not sufficient for the all-Rust goal; the *orchestrator* and
its post-processing steps are Python too.

**[`gftools-builder3`](https://github.com/googlefonts/gftools/) is the vehicle for closing that
gap.** It is the planned **Rust-native builder** that replaces the Python `gftools.builder`
orchestrator: a Rust pipeline that drives fontc directly and performs instancing/fixing/packaging
without a Python interpreter in the loop. We are **not** using it yet — `gflib-build` builds via
`gftools.builder --experimental-fontc` today, which is the pragmatic bridge that gets us up the ladder
to L4 now. But **builder3 is exactly what L5→M5 and ultimately M7 are about**: it is the component
that lets a family cross from "fontc compiles it" (L4) to "no Python anywhere in its build" (L5/L6).
As builder3 matures, `gflib-build` will gain a backend that invokes it in place of
`gftools.builder`, and the recorded-compiler-version machinery (M0) extends naturally to record the
*builder* (builder2 vs builder3) alongside the *compiler* (fontmake vs fontc).

## The per-family "Rust-readiness ladder"

Each family sits at a level; milestones move the library's *distribution* up the ladder.

| Level | Meaning |
|------:|---------|
| **L0** | Doesn't build with any backend |
| **L1** | Builds correctly with **fontmake** (Python path) |
| **L2** | **fontc attempted, fails** — gap recorded (which fontc version, what error) |
| **L3** | fontc builds, but output **≠** fontmake/shipped |
| **L4** | fontc builds, output **equivalent** |
| **L5** | L4 **and** built **with no Python anywhere** — Rust-native orchestrator (`gftools-builder3`), no Python pre-build/deps, no Python post-processing |
| **L6** | L5 **on the latest fontc** (and latest builder3) |

## Milestones (ordered, measurable)

- **M0 — Measurement foundation** *(in progress).* Record, per attempt, the compiler
  (fontc/fontmake), its **exact version** (+ commit hash for dev fontc), and the outcome — for
  **successes and failures**. Nothing below is measurable without it.
- **M1 — Full buildability (any backend).** 100% of *buildable* families (valid source + config)
  produce the expected fonts. Metric: `built / buildable → 100%`. *(The collect_outputs fix,
  build_rules pre-compiles, and venv self-healing all serve this.)*
- **M2 — Complete fontc-gap map.** Every buildable family is *attempted with fontc* and the result
  recorded → a definitive, versioned "what fontc can't build yet, and why." Metric:
  `fontc-attempted / buildable → 100%`. Drives upstream fontc fixes.
- **M3 — fontc equivalence at scale.** Of the families fontc builds, the output is
  byte-identical / acceptably equivalent to fontmake/shipped. Thresholds **50 → 80 → 95 → 100%**.
  Metric: `fontc-equivalent / fontc-built`. *(`--backend both` + the `vs` table is the engine.)*
- **M4 — fontc majority (headline Rust-adoption %).** Families that build *correctly with fontc
  alone* (no fontmake fallback). Thresholds **50 → 80 → 95%** of the library. Metric:
  `fontc-only-correct / buildable`.
- **M5 — Python-free pipeline (`gftools-builder3`).** Families that build through the **Rust-native
  builder3** orchestrator with **no** Python pre-build (`build_rules`) and **no** Python-only deps —
  no Python interpreter anywhere in the build. This is where we **replace `gftools.builder
  --experimental-fontc` (Python orchestrator + fontc) with builder3 (Rust orchestrator + fontc)**.
  ⚠️ Two distinct Python blockers must die for M5: (a) per-repo `build_rules` pre-build scripts, and
  (b) the `gftools.builder` orchestrator itself. **Every `build_rules` pre-build script is a stopgap
  that keeps M1 alive but BLOCKS M5** — the tool tracks Python-dependence as a *migration blocker*,
  not a win. Metric: `builder3-built-Python-free / buildable`.
- **M6 — Latest-fontc / latest-builder3 currency.** Re-validate the M4/M5 set on the **latest**
  fontc **and the latest builder3** (not pinned old ones). Needs easy "point at fontc/builder3
  version X, re-run" + the recorded versions to catch regressions across releases of *either*.
- **M7 — FINAL: 100% Rust.** 100% of the library at **L6** (latest fontc, equivalent output, zero
  Python).

## What this means for the tool's features

1. **Record compiler + version everywhere** (M0) — the foundation for every metric below. As
   `gftools-builder3` lands, extend this to record the **builder** (builder2 vs builder3) too, since
   "which orchestrator" is as load-bearing as "which compiler" for L5/M5.
2. The **stats / migration views** should report the **L0–L6 distribution** and each milestone's %.
3. The **fontc-gap list** (M2) should be exportable / dashboard-able.
4. Treat **Python pre-build *and* the Python orchestrator as tracked blockers** toward M5 — surface
   both, don't celebrate an L4 (fontc-via-`gftools.builder`) as if it were Python-free.
5. Make the **fontc *and* builder3 versions selectable + the build re-runnable** so M6 is a routine
   re-validation. Add a **builder3 backend** alongside `gftools.builder --experimental-fontc` once
   builder3 is usable; fontc-first ordering then becomes builder3-first ordering.
