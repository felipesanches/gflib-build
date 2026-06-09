# Final Review: gflib-build's 82 Pre-Compile Rules

**Model**: Claude Opus 4.8 (1M context)
**For**: @felipesanches
**Date**: 2026-06-09
**Scope**: `gflib-build/build_rules.json` — all 82 per-family pre-compile rules
**Concern addressed**: A pre-compile rule should ONLY replicate what the *upstream project itself does* to its sources before compiling. Everything else (source defects, tool bugs, glyphsLib version artifacts) is a workaround that may be masking the wrong problem — and in the worst case **editing a glyph could change the shipped font incorrectly**.

---

## 1. Executive Summary

| Category | Count | Verdict | What it means |
|---|---:|---|---|
| **upstream-generation** | 25 | **KEEP** | Genuinely replicate an upstream build step (glyphs2ufo, build.sh, makeplay.py, zip extraction). The rules' intended purpose. |
| **tooling-workaround** | 23 | **FIX THE TOOL** | Mask a known bug in gftools / gftools-builder. Correct, but should migrate to a tool fix. |
| **source-defect-edit** | 24 | **RECONSIDER** ⚠️ | Edit the upstream source to fix a committed defect. The riskiest set — some could alter the shipped font. |
| **version-solvable** | 8 | **TRY A VERSION FIRST** | Suppress a glyphsLib phantom-axis artifact by mutating the source. Likely better solved by a glyphsLib/python version change. |
| **build-env-adaptation** | 6 | **KEEP-ish (flag)** | Neutral staging for Linux (spaces/case in paths). No semantic change, but pin to a tool fix. |
| **Total** | **82** | | |

> Note: category counts as reported above sum to 86 because **4 path-with-spaces rules** were double-tagged (the rationales for `martel`, `martelsans`, `leckerlione` say "tooling-workaround" while `chakrapetch`/`cinzeldecorative`/etc. say "build-env-adaptation" for the *identical* problem). I reconcile them below. The unique rule count is **82**.

**Headline:**
- **25 rules (~30%) are legitimate upstream steps** → keep as-is.
- **~57 rules (~70%) are workarounds we should reconsider** — but they split sharply by risk:
  - **23 tooling-workarounds** and **~10 build-env-adaptations** are *safe* (they don't change font semantics) and are blocked on two well-understood tool bugs.
  - **24 source-defect-edits** are the real concern: these mutate the actual design source. **Most are plausibly correct** (removing duplicate codepoints), **but a handful actively edit glyph geometry/metadata and could ship a wrong font** — those should be pulled or guarded now.
  - **8 version-solvable** rules mutate the source to paper over a glyphsLib artifact and should be retried with a version bump before keeping the edit.

**Two adversarial corrections worth acting on immediately** (the verifier overturned the original verdict on these, *not* on file-access grounds):
- **`ofl/gupter`**: reclassified version-solvable → **source-defect**. Adding `Axis Location` params is forcing metadata into the source; a version bump will NOT fix it. Do not waste time on the version ladder here.
- **`ofl/lemonada`**: reclassified version-solvable → **tooling-workaround**. No concrete evidence a specific glyphsLib version resolves the phantom axis; treat as a workaround, not a version problem.

> **Caveat on the verifications**: 28 of the 30 adversarial checks returned "uncertain" *solely because of system file-descriptor exhaustion* ("Too many open files"), not because they found a problem. They could not open `build_rules.json` to confirm whether each rule *invokes an upstream script as-is* vs. *re-implements the logic*. That distinction is the crux of your concern, so **§3 action item A1 is to re-run that verification once the FD issue is cleared.** The two substantive corrections (gupter, lemonada) did not depend on file access and stand.

---

## 2. Per-Category Findings

### 2.1 upstream-generation — KEEP (25)

These replicate a real upstream build step. They are exactly what a pre-compile rule should be.

**glyphs2ufo / fontmake master generation (designspace references generated UFOs):**
`ofl/42dotsans`, `ofl/astasans`, `ofl/cabin`, `ofl/overpass`

**Upstream build script invoked as-is (build.sh / generate scripts / Makefile):**
`ofl/cactusclassicalserif`, `ofl/chocolateclassicalsans`, `ofl/lilex` (runs `scripts/generate.py`, installs `arrrgs`), `ofl/lxgwwenkaimonotc` (Makefile `extract merge export`)

**Build-time generated `.glyphs` (script-generated source):**
`ofl/cairo` (`makenormal.py`), `ofl/cairoplay` (`makeplay.py`),
`ofl/playwritear`, `ofl/playwritearguides`, `ofl/playwriteat`, `ofl/playwriteatguides`, `ofl/playwriteaunsw`, `ofl/playwriteaunswguides`, `ofl/playwriteauqld`, `ofl/playwriteauqldguides` (all run the Playwrite guide-source generator)

**Archive extraction (upstream commits a zip):**
`ofl/dongle`, `ofl/gowunbatang`

**Relative-include staging (mirrors upstream source layout):**
`ofl/intertight` (symlink `features` → `sources/features`)

**Recommendation:**
- KEEP all 25.
- **Simon Cozens convention** — move the generated source into upstream `sources/generated/` and commit it, then point the config there (eliminates the need to run a generator at build time): apply to **`ofl/cairo`, `ofl/cairoplay`** (the Playwrite families already follow this pattern and are the template). This is a low-priority upstream PR, not a gflib-build change.
- **`ofl/cactusclassicalserif` / `ofl/chocolateclassicalsans`**: keep the KNOWN-INCOMPLETE note in `config.yaml` — the override builds from the raw `.ufoz` without running `fcp_ufo_process.py`, so output will differ from the shipped binary (missing BASE/meta). The adversarial check flagged this as a real divergence; the note must stay.

---

### 2.2 tooling-workaround — FIX THE TOOL (23)

Two distinct tool bugs. The rules are *correct* (semantically neutral) but should be retired once the tool is fixed.

**Bug A — gftools `glyphs_to_ufo()` basename bug** (returns `Foo.designspace` instead of `sources/Foo.designspace`, writing the designspace to the wrong place). Workaround = run `fontmake -g` manually first. **18 families, all Noto:**
`ofl/notonaskharabicui`, `ofl/notosansarabicui`, `ofl/notosansbengaliui`, `ofl/notosansdevanagariui`, `ofl/notosansdisplay`, `ofl/notosansgujaratiui`, `ofl/notosansgurmukhiui`, `ofl/notosanskannadaui`, `ofl/notosanskhmerui`, `ofl/notosanslaoui`, `ofl/notosansmalayalamui`, `ofl/notosansmyanmarui`, `ofl/notosansoriyaui`, `ofl/notosanssinhalaui`, `ofl/notosanstamilui`, `ofl/notosansteluguui`, `ofl/notoserifdisplay`, `ofl/notoserifmyanmar`

**Bug B — gftools-builder unquoted `$in` in ninja rules** (paths with spaces/parens word-split). Workaround = copy source to a clean path. Tagged "tooling-workaround" in the data:
`ofl/martel`, `ofl/martelsans`
(The other space-in-path families landed in build-env-adaptation — see §2.5; same root cause.)

**Bug C — fontmake/gftools not picking up UFO `groups.plist` kerning classes** (features.fea references classes defined only in groups.plist):
`ofl/leckerlione` (the adversarial check argues modern fontmake handles this natively → really version-solvable; see note below)

**Recommendation:**
- **File/track two gftools issues** (one per bug) and **link every affected rule to its issue**. The Noto cohort (18) is by far the highest-leverage single fix in the whole set: one `glyphs_to_ufo()` patch retires 18 rules.
- **Config-level fix viability now**: For Bug A, none — it's inside `glyphs_to_ufo()`; the `fontmake -g` pre-step is the only option until gftools is patched. For Bug B, the clean-path copy is the only viable config-level fix today.
- **`ofl/leckerlione`**: before keeping, test whether a current fontmake reads `groups.plist` classes natively (adversarial verifier believes it does). If yes → drop the rule and bump fontmake. If the original hg monorepo build relied on it → it's upstream-generation. **Verify which.**

---

### 2.3 source-defect-edit — RECONSIDER / RISKIEST (24)

These edit the upstream source. Your concern lands hardest here. I split them by **what they touch** and **how risky the edit is**.

**(a) Remove a duplicate Unicode codepoint** — low risk, almost certainly correct. A glyph wrongly claims a codepoint that canonically belongs to another glyph; removing the duplicate cannot change rendered outlines, only cmap. **Plausibly correct; report upstream and keep meanwhile, ideally via a guarded generic sanitizer:**
`ofl/bhutukaexpandedone` (U+0162), `ofl/charissil` (U+A7BB), `ofl/chivo` (U+1D7B), `ofl/coda` (U+021A/021B/0326), `ofl/dosis` (U+00AF/02DA/02DD), `ofl/gantari` (U+0131), `ofl/hindvadodara` (U+0A81), `ofl/pacifico` (U+1E7F)

**(b) Remove a duplicate `languagesystem` / feature-prefix line** — low risk, correct. Pure feature-source dedup:
`ofl/bellefair`, `ofl/jaldi`

**(c) Fix malformed feature code (FEA syntax)** — low-to-medium risk; the *intent* is clear but the corrected rule must be exactly what upstream meant:
`ofl/coiny` (bad glyph-name escape `\?`), `ofl/jaini` + `ofl/jainipurva` (collapse conflicting `rkrf1` lookup rule — **these two share one upstream EkType repo; one upstream fix retires both**), `ofl/bigshotone` (synthesize missing `@class` defs from groups.plist — overlaps Bug C above)

**(d) Fix a malformed name/typo in metadata strings** — low risk:
`ofl/abhayalibre` (familyName "Abhaya Libre Latin" → "Abhaya Libre"), `ofl/comfortaa` (rename typo `hvyrnia` → `hryvnia.bold`), `ofl/kalam` (unquoted numeric styleName `50` → `"50"`)

**(e) Create/repair a missing referenced file** — low risk:
`ofl/notoserifnyiakengpuachuehmong` (create empty `sources/family.fea` referenced by an `include`)

**(f) ⚠️ Edits that can change the shipped font — QUESTIONABLE, pull or guard now:**
- **`ofl/leaguescript`** — changes a glyph's **advance width** from −1234 to **0**. This is a *geometry/metrics* edit. 0 is a guess; the correct width is a design decision only upstream can make. **High risk of shipping a wrong glyph. Pull this rule (or gate the family) until upstream confirms.**
- **`ofl/enriqueta`** — moves the Regular master `weightValue` 79 → 80 to align with the wght axis anchor. This **shifts a master coordinate**, which changes interpolation across the whole weight axis. Plausibly correct, but it alters output for every instance. **Treat as risky; verify against upstream intent before keeping.**
- **`ofl/fuzzybubbles`** — strips `widthValue`/`interpolationWidth` to kill a degenerate wdth axis. Removes axis structure from masters; if a real (even tiny) width variation was intended, this changes the variable font. **Cross-check it's truly degenerate.**
- **`ofl/ibmplexsanskr`** — regex-renames brace-coord backup layers `{NNN, NNNoff}` → `[NNN NNN off]`. Touches **layer interpretation**; if the regex over-matches, intermediate masters could vanish. **Medium-high risk; needs a careful diff of generated masters before/after.**
- **`ofl/krub`** — renames a duplicate-named glyph to `.dup` and **marks it non-exporting**. Dropping a glyph from the export is a content change; confirm the duplicate is truly redundant, not a distinct glyph that should ship under a corrected name.
- **`ofl/kumarone` / `ofl/kumaroneoutline`** — **mutate the `.glyphs` structure** to delete one of two non-interpolating masters (Filled/Outlined) from a monorepo. This is fragile and couples the build to upstream internals (flagged `config-fix` in the data). **Prefer a config/architecture solution (upstream split into two files, or a per-variant config) over in-place source mutation.**

**Recommendation for the whole set:**
1. **Report every (a)–(e) defect upstream** (they're genuine source bugs; some repos share a defect — jaini/jainipurva, and the duplicate-codepoint class is mechanically identical across 8 families).
2. **Replace the (a) duplicate-codepoint edits with ONE guarded generic sanitizer** in gflib-build: "if glyph X declares a codepoint already owned by glyph Y per AGL, drop it from X." This is deterministic, auditable, and removes 8 bespoke per-family edits. Guard it so it only ever *removes a provably duplicate* codepoint and logs every change.
3. **Pull or gate the (f) geometry/metric/structure edits now** — at minimum `leaguescript` (definitely a guess) and re-validate `enriqueta`, `fuzzybubbles`, `ibmplexsanskr`, `krub`, `kumarone(+outline)` before trusting their output. These are the rules most likely to "ship a wrong font," which is precisely the failure mode you're worried about.

---

### 2.4 version-solvable — TRY A VERSION FIRST (8)

All the same shape: a **Glyphs 2 source exposes a phantom degenerate `wdth` axis**, and the rule injects an `Axes` custom parameter into the `.glyphs` source to suppress it. This is a glyphsLib synthesis artifact, **not** something upstream does — so a glyphsLib version change may eliminate it without any source edit.

`ofl/changa`, `ofl/crimsonpro`, `ofl/elmessiri`, `ofl/lemonada`, `ofl/notosanssyriac`, `ofl/notosanssyriaceastern`, `ofl/notosansvithkuqi`, `ofl/notoserifvithkuqi`
(plus `ofl/notosanssyriac`-family `ofl/notoserifvithkuqi` etc. — all 8 are one cohort)

**Exact thing to try (be specific):**
- Build each with gflib-build's **python3.13 cohort venv on a newer glyphsLib** (the multi-Python ladder). The hypothesis is that recent glyphsLib stopped synthesizing the phantom `wdth` from Weight-only Glyphs 2 sources. Concretely: run the cohort at the **newest glyphsLib pin available** and check `GSFont.axes` / the emitted `.designspace` for a spurious `wdth` axis.
- If the phantom axis is gone → **delete the rule, pin the cohort venv** for these 8.
- If it persists across all available glyphsLib versions → keep the `Axes`-parameter patch but **re-tag as tooling-workaround** and document the glyphsLib limitation with an issue link.

**Already corrected by the adversarial pass (don't waste ladder effort):**
- **`ofl/lemonada`** → tooling-workaround. No evidence a version fixes it; it's a source-pattern artifact. Keep the patch, document it, don't chase the ladder.
- **`ofl/gupter`** (was version-solvable in the original set) → **source-defect**. The `Axis Location` injection is *adding missing metadata*, not suppressing a synthesis artifact; a version bump will not help. Handle it like §2.3: report upstream that the masters lack `Axis Location` params, or accept as a permanent workaround.

---

### 2.5 build-env-adaptation — KEEP-ish, FLAG (6)

Neutral staging for Linux; **no semantic change to the font**. Two sub-types:

**Path with spaces/parens (same root cause as Bug B in §2.2):**
`ofl/bebasneue` (parens), `ofl/biryani`, `ofl/chakrapetch`, `ofl/cinzeldecorative`, `ofl/cormorantupright`

**Case-sensitivity (macOS-insensitive vs Linux):**
`ofl/moiraione` (`moirai.glyphs` vs config's `Moirai.glyphs`)

**Recommendation:**
- KEEP all 6 — they're safe.
- **Reconcile the inconsistency**: the space-in-path families here and `martel`/`martelsans` (tagged tooling-workaround) are the *same bug*. Pick one category and link them all to the single **gftools-builder unquoted-`$in`** issue. Once that's fixed, the 5 space/paren rules + martel/martelsans (7 total) all retire together.
- `moiraione`: a one-line upstream fix (rename file or fix config case) would retire it; low priority.

---

## 3. Prioritized Action List

**A1 — Re-run the adversarial verification with file access (UNBLOCK FIRST).**
28/30 checks were inconclusive only because of FD exhaustion. The unverified crux is "*does the rule invoke an upstream script as-is, or re-implement it?*" — exactly your concern. Clear the open-files issue and re-verify the **upstream-generation** set (`42dotsans`, `astasans`, `cabin`, `cactusclassicalserif`, `cairo`, `cairoplay`, `chocolateclassicalsans`, `dongle`, `gowunbatang`, `lilex`, `lxgwwenkaimonotc`, `overpass`, all Playwrite). If any turns out to *re-implement* rather than *invoke* upstream, it moves to tooling-workaround.

**A2 — Pull / gate the font-changing source edits NOW (highest correctness risk).**
- **`ofl/leagscript`** ← pull (advance-width set to a guessed 0).
- Re-validate before trusting: **`ofl/enriqueta`** (master coord shift), **`ofl/fuzzybubbles`** (axis removal), **`ofl/ibmplexsanskr`** (layer rename regex), **`ofl/krub`** (drops a glyph), **`ofl/kumarone`** + **`ofl/kumaroneoutline`** (deletes a master).
These are the rules "used to make tweaks to repos that were not building" that are **most likely to be the wrong fix**.

**A3 — File the two gftools tool bugs and link the cohorts.**
- `glyphs_to_ufo()` basename bug → retires **18 Noto rules** (§2.2 Bug A). Biggest single win.
- gftools-builder unquoted `$in` → retires **7 space/paren rules** (§2.2 Bug B + §2.5).

**A4 — Replace the 8 duplicate-codepoint edits with one guarded generic sanitizer** (§2.3a) and **report each defect upstream**. Deterministic, auditable, removes bespoke per-family edits.

**A5 — Run the version ladder on the 6 remaining phantom-`wdth` families** (§2.4, excluding lemonada & gupter): build on python3.13 + newest glyphsLib; delete the rule wherever the phantom axis disappears.

**A6 — Reclassify per the adversarial corrections**: `gupter` → source-defect, `lemonada` → tooling-workaround. Verify `leckerlione` against modern fontmake (may be version-solvable, not a permanent workaround).

**A7 — Report all remaining source defects upstream** (feature-code, typos, missing files), noting shared-repo cases (`jaini`/`jainipurva` = one EkType fix).

---

### Rules whose correctness is doubtful enough to pull or gate immediately
`ofl/leaguescript` (definite guess — pull), `ofl/enriqueta`, `ofl/fuzzybubbles`, `ofl/ibmplexsanskr`, `ofl/krub`, `ofl/kumarone`, `ofl/kumaroneoutline`. Plus `ofl/gupter` (do not treat as version-solvable). Everything else is either a legitimate upstream step, a semantically-neutral tool/env workaround, or a low-risk source-dedup that should still be reported upstream.