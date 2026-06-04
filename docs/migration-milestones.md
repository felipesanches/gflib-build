# Python → Rust migration milestones

**North star.** Build the **entire Google Fonts library** reproducibly and correctly with the
**latest `fontc`** and **zero Python in the build pipeline** (no gftools-builder2 / fontmake, no
Python pre-build scripts, no Python-only deps) — a **fully-Rust** pipeline.

**Why `gflib-build` tolerates fontmake / old fontc today.** We are not there yet. The tool
deliberately uses whatever compiler works *right now* — **as long as it records the compiler and
its exact version for every build attempt, success or failure.** That recorded data is what lets us
*morph gradually* toward the all-Rust future and measure every intermediate step. **Every feature
should be designed with these milestones in mind** (this is the concrete meaning of spec 12).

## The per-family "Rust-readiness ladder"

Each family sits at a level; milestones move the library's *distribution* up the ladder.

| Level | Meaning |
|------:|---------|
| **L0** | Doesn't build with any backend |
| **L1** | Builds correctly with **fontmake** (Python path) |
| **L2** | **fontc attempted, fails** — gap recorded (which fontc version, what error) |
| **L3** | fontc builds, but output **≠** fontmake/shipped |
| **L4** | fontc builds, output **equivalent** |
| **L5** | L4 **and** needs **no Python** pre-build/deps (source is Rust-consumable) |
| **L6** | L5 **on the latest fontc** |

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
- **M5 — Python-free pipeline.** Families that build with **no** Python pre-build (`build_rules`)
  and **no** Python-only deps. ⚠️ **Every `build_rules` pre-build script is a stopgap that keeps M1
  alive but BLOCKS M5** — the tool should track Python-dependence as a *migration blocker*, not a win.
- **M6 — Latest-fontc currency.** Re-validate the M4/M5 set on the **latest** fontc (not a pinned
  old one). Needs easy "point at fontc version X, re-run" + the recorded version to catch
  regressions across fontc releases.
- **M7 — FINAL: 100% Rust.** 100% of the library at **L6** (latest fontc, equivalent output, zero
  Python).

## What this means for the tool's features

1. **Record compiler + version everywhere** (M0) — the foundation for every metric below.
2. The **stats / migration views** should report the **L0–L6 distribution** and each milestone's %.
3. The **fontc-gap list** (M2) should be exportable / dashboard-able.
4. Treat **Python pre-build as a tracked blocker** toward M5 — surface it, don't celebrate it.
5. Make the **fontc version selectable + the build re-runnable** so M6 is a routine re-validation.
