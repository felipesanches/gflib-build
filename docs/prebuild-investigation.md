# Pre-build investigation — failed-build families

Starting point: the families that failed **at the `gftools.builder` step** in the previous full run
(42 of 138 failures; the other 96 died earlier at venv/clone steps and are addressed by the venv
self-healing work, not by pre-compile rules). Each repo was interpreted (Makefile / build.sh /
build.py / `.github/workflows` / README) and any proposed fix was verified against the committed
files at the pinned commit. **Iterate**: re-run, see what still fails at the build step, investigate.

## ✅ Pre-build rules added to `build_rules.json` (verified)

| Family | Cause | Pre-build |
|---|---|---|
| `ofl/cairo` | config wants generated `CairoNormal.glyphs` | `makenormal.py` |
| `ofl/cairoplay` | config wants generated `CairoPlay.glyphs` | `makeplay.py` |
| `ofl/lilex` | Makefile `generate` injects OT features into the `.glyphs`; plain build skips it | `scripts/generate.py … generate` |
| `ofl/lxgwwenkaimonotc` | CJK: `build: extract merge export` — sources under `sources/build/` are generated | `extract.py` + `merge.py` |
| `ofl/moiraione` | config refs `Moirai.glyphs`, file is `moirai.glyphs` (case; fails on Linux) | `cp` correctly-cased name |

## ⏳ Deferred — promising but the command isn't fully verifiable offline

- **`ofl/k2d`** — config builds from `source/instance_ufos/*.ufo.json` (instances) that must be
  generated from `source/K2D.glyphs`. Likely `fontmake -o ufo -i -g …`, but the exact output
  naming/format (`.ufo.json`) is uncertain — a wrong command could produce a *bad* font. Verify by
  running fontmake on the source before adding.
- **`ofl/orbit`** — `Unknown name language: KOR`: an invalid uppercase language tag in the source.
  A `sed KOR→kor` patch may fix it, but it's unclear whether the offending tag is in the feature
  code or a name-table parameter. Verify the exact location first.
- **`ofl/yrsa`** — Yrsa is *subset* from Rasa (`fontmake -g … -o ufo` then feature/Unicode
  subsetting per `build.sh`). A plain UFO extraction won't reproduce the shipped Yrsa; needs the
  full subsetting pipeline. Complex.

## ⚠️ NOT a pre-build problem — bigger wins live elsewhere

### Output-collection mismatch (a **gflib-build** fix, not a rule)
Many `produced no expected font files` failures are families whose build **succeeds** but whose
output font isn't matched by gflib-build's `collect_outputs` (name/path differs from the shipped
binary). Confirmed for `ofl/anekgurmukhi`, `ofl/tiltprism`, `ofl/peddana` (and by extension the
`appajid` Telugu set `lakkireddy`/`mallanna`), `ofl/bigshoulderstextsc` /
`bigshouldersinlinedisplaysc`. Strongly suspected for most other `produced no expected font files`
cases (`blinker`, `hindkochi`, `darumadropone`, `lalezar`, `blakahollow`, `goudybookletter1911`,
`londrinasketch`, `redrose`, `rubikscribble`, `rubikburned`, `sumana`, `gentiumplus`). **Fixing
`collect_outputs` to match built fonts more robustly would recover ~15–20 families at once — higher
value than any individual rule.** Next iteration.

### Config should point elsewhere (override config, not a rule)
- **`ofl/khula`** — config points at `Khula_superpolator.sp3` (Superpolator, unbuildable) but the
  repo has committed UFO masters. Fix = an override `config.yaml` pointing at the UFOs.

### Complex external dependency
- **notofonts cluster** (`notosanslycian`, `cherokee`, `hatran`, `newa`, `rejang`, `takri`,
  `notonastaliqurdu`) — configs use `includeSubsets: from: Noto Sans`, pulling subset data from
  Noto Sans during the build. Needs that dependency available; not a simple pre-compile.

### Incompatible source formats (skip — per the VFB/SFD policy)
- `ofl/iceland` (`.vfb`, FontLab), `ofl/dongle` (`.zip`).

### Genuine source bugs (a pre-compile can't help)
- `ofl/alegreyasc` (cyclical component reference), `ofl/aoboshione` (NoneType operand),
  `ofl/bhutukaexpandedone` (duplicate Unicode mapping).

### System dependency
- `ofl/eduauvicwantdots` — needs `hb-subset` (HarfBuzz) on the system.
