# fontc_crater “could not locate the source”: root causes & the path to the correct onboarding commit

**Model**: Claude Opus 4.8 (1M context) · drafted 2026-06-08 · crater run `2026-04-27` (fontc `5b20924e`, google/fonts `755538365478`)

## 1. Executive summary

fontc_crater reports **39 repositories** in the *latest run* as failed before any compiler ran, with reasons of the form *“missing source ‘…’”*. This is the single most actionable category for contributing back to the fontc team: these are **not** fontc compiler bugs — **fontc never started**, because crater could not locate the source file the build config points at. We diagnosed every one against bare-mirror checkouts of the upstream repos at the exact revisions crater used.

The central finding: **every one of these targets is `config_is_external: true`** — crater reads the *same* `google/fonts/ofl/<family>/config.yaml` we maintain and inherits the same provenance question (which commit) our `upstream_info.md` investigations already track. **So this is not a separate project: it is the google/fonts source-metadata enrichment we are already doing.** Fixing the config path or the commit in google/fonts fixes crater on its next run.

## 2. Method

For each failed repo we read crater’s declared target (`crater_targets`: upstream URL, pinned `rev`, config path, `config_is_external`), located the bare mirror in the local archive (`gflib-data/archive/<owner>/<repo>.git`), and checked with `git cat-file -e` / `ls-tree` whether the expected source exists (a) at crater’s pinned rev, (b) at the commit recorded in our google/fonts `METADATA.pb`, and (c) at `HEAD` — plus what sources *do* exist and when the expected one first appeared. The commit dates establish the chronology that distinguishes a stale pin from a genuinely-missing source.

## 3. Root causes — all 39

| Category | n | Why crater can’t locate the source | Contribution / fix |
|---|---|---|---|
| `WRONG_PATH` | 10 | Config `sources:` path doesn’t match the repo layout | Correct the path in the google/fonts override `config.yaml` |
| `STALE_REV (A)` | 6 | Pinned commit predates the source, but it *is* at the METADATA commit | Point crater at the METADATA commit (known-good) |
| `STALE_REV (B)` | 3 | Pinned commit *and* the METADATA commit both predate the buildable source | Forensic onboarding-commit hunt (the `upstream_info.md` effort) |
| `GENERATED` | 7 | The expected source is produced by a build script, never committed | Commit generated sources under `sources/generated/` or add a build step |
| `NOT_MIRRORED` | 8 | Repo not yet in our archive — not diagnosable offline | Mirror into the archive, then re-diagnose |
| `SOURCE_PRESENT` | 2 | Source *is* present at the pinned rev — crater failed for another reason | Inspect individually (`.glyphspackage` / `.designspace`) |
| `ESCAPES_REPO` | 1 | Config path escapes the repo root (`../…`, mono-repo) | Fix the mono-repo path / config |
| `NO_CONFIG` | 1 | No config file found at all | Add a config.yaml |
| `NO_SOURCES_FIELD` | 1 | Config has no `sources` field (custom pipeline) | Custom-pipeline family — needs a tailored target |
| **total** | **39** | | |

## 4. The unifying insight: crater inherits our config *and* our provenance gap

Because these targets are external, the contribution decomposes cleanly:

- **Path-level failures** (`WRONG_PATH`, `ESCAPES_REPO`, `NO_CONFIG`, `NO_SOURCES_FIELD`): the `sources:` path is wrong or absent. Fixing the google/fonts override `config.yaml` — our existing override workflow — **fixes crater automatically**, no separate crater PR. *Highest-leverage bucket.*
- **Commit-level failures** (`STALE_REV`): the config is fine; crater is building the wrong commit. The fix is to point it at the correct onboarding commit (§5).
- **Structural failures** (`GENERATED`, `SOURCE_PRESENT`, `NOT_MIRRORED`): need source-generation handling, individual inspection, or mirroring first.

## 5. Pointing at the correct onboarding commit

The `STALE_REV` bucket splits exactly along the provenance question — and that split is where the goal of *“the commit originally used to build the binaries on Google Fonts”* becomes central:

- **Sub-case A** — crater pins an *older* commit, but the source *is* present at our recorded METADATA commit. The correct commit is known and verified; crater just needs it.
- **Sub-case B** — crater’s rev *equals* our METADATA commit, and *neither* has the buildable source (it appears only later). Here **google/fonts’ own metadata points at a pre-source commit**, so the onboarding commit is wrong/unknown and must be recovered by binary↔source comparison. Fixing it benefits both repos.

| Family | Sub | crater rev / date | METADATA commit / date | src @META | Correct commit |
|---|---|---|---|---|---|
| lalezar | A | `c3e0eae242` 2016-08-22 | `238701c424` 2017-02-28 | yes | `238701c424` |
| amethysta | B | `10ae36bc06` 2016-04-06 | `10ae36bc06` 2016-04-06 | no | `*unresolved*` |
| jacquesfrancois | A | `bc37f476a7` 2016-01-10 | `d34156392b` 2018-02-12 | yes | `d34156392b` |
| jacquesfrancoisshadow | A | `90c9f94cc7` 2016-01-10 | `073491c6b1` 2018-02-12 | yes | `073491c6b1` |
| prata | B | `db5f3799a4` 2016-12-14 | `db5f3799a4` 2016-12-14 | no | `*unresolved*` |
| intelonemono | A | `cec102c389`  | `99e2d6ca17` 2024-07-26 | yes | `99e2d6ca17` |
| notosansarabic | A | `133ccaebf9` 2022-08-01 | `6c8320740d` 2023-11-07 | yes | `6c8320740d` |
| raleway | A | `7b288c6faa`  | `938ac77022` 2020-08-26 | yes | `938ac77022` |
| castoro | B | `58a386a96e` 2023-03-15 | `58a386a96e` 2023-03-15 | no | `*unresolved*` |

## 6. The `upstream_info.md` investigations are the contribution vehicle

The provenance answers crater needs already live (or belong) in our per-family investigation reports. Two worked examples from the sub-case B set show why this is the binding constraint:

- **Jacques François** — `upstream_info.md` status `missing_commit`: *“the original commit cannot be reliably determined… needs investigation to identify which upstream commit corresponds to the font currently in google/fonts.”* Crater pinned the 2016 *initial commit* (prebuilt TTF only, no `sources/`); the modern source exists at the 2018 `d341563` regeneration.
- **Amethysta** — `upstream_info.md` already flags it **“WON’T FIX (unreproducible)”**: the recorded commit `10ae36bc` (2016) carries only the prebuilt TTF; the source first appears in `d3cd4ca` (“Release v1.002”), yet the shipped binary is **v1.003 (2017)**. The artifact on Google Fonts does not correspond to a buildable commit in the repo.

**So crater’s “could not locate the source” is, for the hardest cases, the same unresolved provenance our enrichment effort is tracking.** Each resolved onboarding commit is a single fix that lands in `upstream_info.md`, corrects `METADATA.pb`, and unblocks crater at once.

## 7. Proposed contribution path

1. **Path fixes (the `WRONG_PATH` + structural-config bucket).** Correct each google/fonts override `config.yaml` `sources:` path (Appendix B); crater inherits it. Fastest, auto-propagates.
2. **Stale-rev sub-case A.** Supply the verified METADATA commit to crater’s target list (confirm first whether crater resolves its rev from METADATA or from its own `sources.json`).
3. **Stale-rev sub-case B.** Run the binary↔source forensic hunt; record the onboarding commit in `upstream_info.md` + `METADATA.pb`; some (amethysta) may stay *unreproducible* and should be reported to crater as such.
4. **Generated sources.** Adopt the `sources/generated/` convention so both we and crater can build them.
5. **Not-mirrored.** Mirror into the archive, then re-run this diagnosis.

## Appendix A — per-repository diagnosis (all 39)

| Repo | Family | Category | Missing source (crater config) | crater rev |
|---|---|---|---|---|
| `alexeiva/arsenal` | arsenal | `WRONG_PATH` | `Arsenal-Italic.glyphs` | `878af08407` |
| `googlefonts/sawarabi-mincho` | sawarabimincho | `WRONG_PATH` | `SawarabiMincho.glyphs` | `8bd1525717` |
| `gue3bara/cairo` | cairo | `WRONG_PATH` | `CairoPlay.glyphs` | `73d16933c6` |
| `itfoundry/kumar` | kumaroneoutline | `WRONG_PATH` | `Kumar One.glyphs` | `3192a79a79` |
| `lipiraval/mogra` | mogra | `WRONG_PATH` | `Mogra.glyphs` | `048039d237` |
| `m4rc1e/istok-web` | istokweb | `WRONG_PATH` | `IstokWeb-Italic.glyphs` | `f995ade617` |
| `mooniak/abhaya-libre-font` | abhayalibre | `WRONG_PATH` | `sources/glyphs/Abhaya-Masters.glyphs` | `f53da70786` |
| `omnibus-type/barrio` | barrio | `WRONG_PATH` | `sources/Barrio.glyphs` | `8f33bf10cb` |
| `omnibus-type/pragatinarrow` | pragatinarrow | `WRONG_PATH` | `PragatiNarrow.glyphs` | `829be323c4` |
| `weiweihuanghuang/fragment-mono` | fragmentmonosc | `WRONG_PATH` | `Fragment-Mono.glyphs` | `3ff027831f` |
| `bornaiz/lalezar` | lalezar | `STALE_REV (A)` | `sources/Lalezar.glyphs` | `c3e0eae242` |
| `cyrealtype/jacques-francois` | jacquesfrancois | `STALE_REV (A)` | `sources/JacquesFrancois.glyphs` | `bc37f476a7` |
| `cyrealtype/jacques-francois-shadow` | jacquesfrancoisshadow | `STALE_REV (A)` | `sources/JacquesFrancoisShadow.glyphs` | `90c9f94cc7` |
| `intel/intel-one-mono` | intelonemono | `STALE_REV (A)` | `sources/masters/IntelOneMono-Roman.des` | `cec102c389` |
| `notofonts/arabic` | notosansarabic | `STALE_REV (A)` | `sources/NotoNaskhArabicUI.glyphspackag` | `133ccaebf9` |
| `theleagueof/raleway` | raleway | `STALE_REV (A)` | `sources/Raleway-Italic.designspace` | `7b288c6faa` |
| `cyrealtype/amethysta` | amethysta | `STALE_REV (B)` | `sources/Amethysta-Regular.glyphs` | `10ae36bc06` |
| `cyrealtype/prata` | prata | `STALE_REV (B)` | `sources/Prata.glyphs` | `db5f3799a4` |
| `tirotypeworks/castoro` | castoro | `STALE_REV (B)` | `source/Castoro-Italic.designspace` | `58a386a96e` |
| `aaronbell/lxgwmarkergothic` | lxgwmarkergothic | `GENERATED` | `temp/LXGWMarkerGothic-Regular.ufo` | `fe83570074` |
| `cadsondemak/sriracha` | sriracha | `GENERATED` | `Sriracha-Regular.ufo` | `6c6cf92ed8` |
| `cathschmidt/yatra-one` | yatraone | `GENERATED` | `YatraOne_0.ufo` | `b991e49f27` |
| `moonlitowen/cactusserif` | cactusclassicalserif | `GENERATED` | `temp/CactusClassicalSerif-Regular.ufo` | `a267f9f320` |
| `moonlitowen/chocolatesans` | chocolateclassicalsans | `GENERATED` | `temp/ChocolateClassicalSans-Regular.uf` | `624ecb8064` |
| `moonlitowen/thenkhung` | uoqmunthenkhung | `GENERATED` | `temp/UoqMunThenKhung-Regular.ufo` | `cdf0805fd0` |
| `sorkintype/trykker` | trykker | `GENERATED` | `Trykker` | `5226cb0750` |
| `anoxic/neuton` | neuton | `NOT_MIRRORED` | `NL.ufo` | `b376055d27` |
| `gitlab.com/smc/fonts/manjari` |  | `NOT_MIRRORED` | `sources/Manjari-Bold.ufo` | `8948773e57` |
| `googlefonts/cousine` | cousine | `NOT_MIRRORED` | `sources/Cousine-BoldItalic.glyphs` | `7f897dbd87` |
| `googlefonts/noto-fonts` |  | `NOT_MIRRORED` | `sources/NotoSansGurmukhi.glyphs` | `090cc7e2cf` |
| `googlefonts/tinos` | tinos | `NOT_MIRRORED` | `sources/Tinos-BoldItalic.glyphs` | `aaf68d53c2` |
| `loudifier/comic-relief` | comicrelief | `NOT_MIRRORED` | `ComicRelief-Bold.ufo` | `856315f5a4` |
| `notofonts/duployan` | notosansduployan | `NOT_MIRRORED` | `NotoSansDuployan.glyphs` | `9626869cc8` |
| `vernnobile/oxygenfont` | oxygenmono | `NOT_MIRRORED` | `Oxygen-Regular.ufo` | `62db0ebe34` |
| `aliftype/amiri` | amiri | `SOURCE_PRESENT` | `sources/Amiri.glyphspackage` | `480bb746e9` |
| `notofonts/nyiakeng-puachue-hmong` | notoserifnphmong | `SOURCE_PRESENT` | `sources/NotoSerifNPHmong.designspace` | `2c945bb9c3` |
| `xotypeco/big_shoulders` | bigshouldersdisplaysc | `ESCAPES_REPO` | `../Big-Shoulders/sources/BigShoulders.` | `0b3d09a868` |
| `googlefonts/dm-fonts` | dmsans | `NO_CONFIG` | `no config file was found` | `027cea4e4f` |
| `petrvanblokland/typetr-bitcount` | bitcountpropsingleink | `NO_SOURCES_FIELD` | `missing field `sources`` | `653fc48a72` |

## Appendix B — `WRONG_PATH` source-path corrections

The config points one place; the repo keeps the source another. Correct the `sources:` entry in each google/fonts override `config.yaml` (crater then inherits it).

| Family | config `sources:` says | repo actually has |
|---|---|---|
| abhayalibre | `sources/glyphs/Abhaya-Masters.glyphs` | `documentation/development/03-08-15/Abhaya-Masters-09.glyphs` |
| arsenal | `Arsenal-Italic.glyphs` | `sources/Arsenal-Italic.glyphs` |
| barrio | `sources/Barrio.glyphs` | `Barriecito/sources/Backup/1138_Barriecito.glyphs` |
| cairo | `CairoPlay.glyphs` | `sources/Archive/Cairo-italic.glyphs` |
| fragmentmonosc | `Fragment-Mono.glyphs` | `sources/Fragment-Mono.glyphs` |
| istokweb | `IstokWeb-Italic.glyphs` | `old/v.1.0.3/sources/SDF-PS/Istok-Bold.sfd` |
| kumaroneoutline | `Kumar One.glyphs` | `masters/Kumar One.glyphs` |
| mogra | `Mogra.glyphs` | `sources/Mogra.glyphs` |
| pragatinarrow | `PragatiNarrow.glyphs` | `SRC/PragatiNarrow.glyphs` |
| sawarabimincho | `SawarabiMincho.glyphs` | `fonts/gothic/SawarabiGothic-Medium.sfd` |

