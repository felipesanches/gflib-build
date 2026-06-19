//! Failure taxonomy — a VERBATIM port of the Python `categorize_failure` / `is_transient_clone_error`
//! and the AUTO_RETRY / IN_BUILD_RETRY category sets, so the Rust port's "failures by cause" matches
//! the Python tool exactly (same cause strings, same hints, same self-heal retry semantics). This is
//! R1 of PARITY_PLAN.md: an insight that must survive a Python→Rust migration unchanged.

/// Network hiccups a clone usually survives on a second try (kept SPECIFIC so a build error whose log
/// merely contains "500" isn't mistaken for a transient fetch and retried forever).
const TRANSIENT_CLONE_ERRORS: [&str; 14] = [
    "invalid index-pack output", "fetch-pack", "early eof", "rpc failed", "unexpected disconnect",
    "the remote end hung up", "connection reset", "connection timed out", "operation timed out",
    "could not resolve host", "gnutls_handshake", "ssl connect error", "tls handshake",
    "returned error: 50",
];

pub fn is_transient_clone_error(err: &str) -> bool {
    let low = err.to_lowercase();
    TRANSIENT_CLONE_ERRORS.iter().any(|s| low.contains(s)) || low.contains("returned error: 429")
}

/// Map a failure message to a short CAUSE + an actionable HINT (verbatim from Python).
pub fn categorize_failure(error: &str) -> (&'static str, &'static str) {
    let low = error.to_lowercase();
    if low.contains("needs python pre-build") {
        return (
            "needs Python pre-build",
            "the Rust-only policy refused a Python pre-build step — authorize Python for this family/dependency, or port the pre-build to shell or Rust",
        );
    }
    if (low.contains("no such file or directory") || low.contains("permission denied"))
        && (low.contains("failed to execute") || low.contains("taskset") || low.contains("/tools/"))
    {
        return (
            "builder binary missing",
            "the builder/compiler binary is missing or not runnable at its provisioned path — provision the Rust toolchain (it auto-installs to <data-dir>/tools/), or set --builder3-bin / --fontc-bin to a built binary, then retrigger",
        );
    }
    if low.contains("could not launch builder") || low.contains("no such file or directory: 'fontmake'") {
        return (
            "build launcher error",
            "the venv python/fontmake wasn't found via a relative build path — fixed by resolving the venv to an absolute path; re-attempted automatically",
        );
    }
    if low.contains("no module named 'gftools'")
        || low.contains("no module named gftools")
        || low.contains("could not launch builder")
        || low.contains("module specification")
    {
        return (
            "broken dependency venv",
            "the cohort venv had a failed install — it's rebuilt from scratch and re-attempted automatically each time you start the build",
        );
    }
    if low.contains("missing system library") {
        return (
            "missing system library",
            "a package built from source needs a native -dev library (e.g. apt install libcairo2-dev pkg-config) — self-heal can't install system pkgs",
        );
    }
    if low.contains("dependency conflict") {
        return (
            "dependency conflict",
            "a cohort dep needs a different version of a base tool; the conflicting base pin is auto-relaxed on retry — if it persists the repo pins an unbuildable combination",
        );
    }
    if low.contains("resolution too deep") {
        return (
            "pip resolution too deep",
            "the repo's requirements make pip backtrack endlessly — needs tighter constraints; not auto-fixable",
        );
    }
    if low.contains("setuptools") || low.contains("pkg_resources") {
        return (
            "build needs setuptools",
            "an sdist needs setuptools/pkg_resources at build time — now seeded into every venv; a retry should clear it",
        );
    }
    if low.contains("base requirements file not found") || low.contains("no build requirements") {
        return (
            "misconfigured requirements",
            "a stale base-requirements path made the venv install nothing — fixed by re-deriving the bundled file; just retry",
        );
    }
    if low.contains("pip install") || low.starts_with("venv:") {
        return (
            "dependency install failed",
            "pip couldn't satisfy the cohort requirements; stale pins are auto-relaxed — see the cohort's .install.log",
        );
    }
    if is_transient_clone_error(error) {
        return (
            "transient fetch error",
            "a network hiccup while cloning — retried automatically; re-run to try the rest",
        );
    }
    if low.contains("mirror absent") {
        return (
            "repo not mirrored",
            "turn on 'populate archive' (or --mirror-missing) so the upstream repo is cloned into the archive",
        );
    }
    if low.contains("not in mirror") {
        return (
            "stale archive mirror",
            "the recorded commit isn't in the local mirror — run git remote update on that repo in the archive",
        );
    }
    if low.contains("mirror clone failed") || low.contains("repository not found") {
        return ("repo unreachable", "the upstream repo may be private, renamed, or removed");
    }
    if low.contains("harness error") {
        return ("internal/transient I/O", "a transient filesystem error — usually clears on re-run");
    }
    if low.contains("timed out") || low.contains("timeout") {
        return ("build timed out", "raise or disable the per-build timeout");
    }
    if low.contains("names don't match shipped") || low.contains("produced no expected font files") {
        return (
            "output name mismatch",
            "the build ran but produced none of the shipped filenames — the upstream source builds different names/axes than google/fonts ships (compare the built names against METADATA.pb)",
        );
    }
    // ---- fontc / source-level build failures (the fontc-gap signal, split out of "build error") ----
    // fontc can't read the .ufo.json the builder2 (gftools.builder) pipeline serializes — only real
    // UFO directories. Pure builder3, or a newer fontc, sidesteps it.
    if low.contains("only ufo (directory)") || low.contains(".ufo.json") {
        return (
            "fontc: unreadable .ufo.json source",
            "the builder2 (gftools.builder) pipeline serialized sources to .ufo.json, which this fontc can't read — set orchestrator=builder3 (pure fontc, no Python preprocessing) or use a newer fontc",
        );
    }
    // gftools-builder3 couldn't order a family's subset-inclusion (AddSubset/includesubsets) graph
    if low.contains("cycle detected") || low.contains("not a valid dag") || low.contains("cyclic") {
        return (
            "builder3: cyclic source graph",
            "gftools-builder3 found a cycle ordering this family's subset-inclusion (AddSubset) dependencies — a builder3 limitation; builder2+fontc or fontmake builds it",
        );
    }
    // fontc's FEA (feature-file) parser rejected OpenType features fontmake's compiler accepts
    if low.contains("fea parsing failed") || low.contains("fea compilation") {
        return (
            "fontc: FEA parse error",
            "fontc's feature-file (fea-rs) parser rejected this family's OpenType features — a fontc-vs-fontmake gap; open the log for the specific FEA error",
        );
    }
    // a designspace axis with no mapping at the default — fontc needs the avar/mapping fontmake infers
    if low.contains("missing mapping on") {
        return (
            "fontc: axis-mapping gap",
            "a designspace axis has no mapping at the default location — fontc needs the explicit axis/avar mapping that fontmake tolerates implicitly",
        );
    }
    // a glyph's masters don't interpolate (point/contour structure differs across masters)
    if low.contains("interpolation-incompatible") || low.contains("incompatible paths") {
        return (
            "interpolation-incompatible masters",
            "a glyph's masters have incompatible outlines (point/contour structure differs across masters) — the source must be fixed so all masters interpolate",
        );
    }
    // the config passes a fontmake-only option fontc doesn't implement (e.g. --subroutinizer)
    if low.contains("unknown fontmake arg") {
        return (
            "fontc: unsupported fontmake option",
            "the config passes a fontmake-only option fontc doesn't implement — drop it from the config, or build this family with fontmake",
        );
    }
    // a glyph referenced in contents.plist has no .glif on disk — the UFO source is incomplete
    if low.contains("gliflib") || (low.contains(".glif") && low.contains("does not exist")) {
        return (
            "incomplete UFO source (missing .glif)",
            "a glyph in the UFO's contents.plist has no .glif file on disk — the upstream source is incomplete, or a pre-build step that should generate it didn't run",
        );
    }
    // fontmake's source-prep (instance/UFO generation) failed before any compile
    if low.contains("fontmakeerror") || low.contains("writing ufo source failed") {
        return (
            "fontmake source-prep failed",
            "fontmake's instance/UFO generation (the builder2 preprocessing step) failed before compiling — open the log; a 'directory not empty' here can be a transient fs race that clears on retry",
        );
    }
    // a malformed/incomplete build config (e.g. a recipe missing its 'sources' key) — require config
    // context so an unrelated KeyError deep in a source's Python isn't mislabeled as a config problem
    if low.contains("keyerror")
        && (low.contains("sources") || low.contains("recipe") || low.contains("config"))
    {
        return (
            "malformed build config",
            "the build config (config.yaml / recipe) is missing a required key (e.g. 'sources') — the recipe is malformed or incomplete",
        );
    }
    // ---- gftools-builder3 build failures, split out of the generic "builder3 error" from real R0 data
    // (python_policy=off ran the whole library through builder3; these are the dominant Rust-only causes) ----
    // a fontc/builder3 PANIC (internal bug, not a source problem) — surface distinctly so it becomes an
    // upstream fontc issue. (Checked first: a panic can co-occur with build-operation context.)
    if low.contains("internal error: entered unreachable")
        || low.contains("rust_backtrace")
        || low.contains("panicked at")
    {
        return (
            "builder3: internal panic",
            "fontc/gftools-builder3 panicked (an internal bug, not a source problem) — capture the family and report it upstream; a different fontc may fix it",
        );
    }
    // the config selects a recipe provider builder3 doesn't implement (e.g. fontprimer, a Python proofing
    // recipe). MUST precede the generic "could not parse config" bucket — the message contains both.
    if low.contains("recipeprovider") && (low.contains("unknown variant") || low.contains("unknown recipe")) {
        return (
            "builder3: unsupported recipe provider",
            "the build config selects a recipe provider gftools-builder3 doesn't implement (e.g. fontprimer) — needs a config fix, or Python (builder2) for that recipe",
        );
    }
    // a designspace the Rust source loader can't parse (e.g. an unsupported conditionset)
    if low.contains("parsing designspace") || (low.contains("designspace") && low.contains("conditionset")) {
        return (
            "builder3: designspace parse error",
            "gftools-builder3's source loader couldn't parse the designspace (e.g. an unsupported conditionset) — a builder3 gap; builder2/fontmake may build it",
        );
    }
    // a source file type builder3 has no loader for (.glyphs/.ufo variants etc.)
    if low.contains("wrong convertor for file") || low.contains("unknown file type for file") {
        return (
            "builder3: unsupported source format",
            "gftools-builder3 has no loader for this source file type (wrong/unknown convertor) — a builder3 source-format gap; builder2/fontmake may handle it",
        );
    }
    // the build config won't parse against builder3's schema (an unsupported/older config shape)
    if low.contains("could not parse config") || low.contains("did not match any variant") || low.contains("data did not match") {
        return (
            "builder3: unparseable build config",
            "gftools-builder3 couldn't parse the build config against its schema (an unsupported/older config shape) — needs a config fix or an override config.yaml",
        );
    }
    // a build operation referenced a file that isn't there (missing source / un-generated intermediate) —
    // often the symptom of a skipped Python pre-build step under python_policy=off
    if low.contains("while building") && low.contains("no such file or directory") {
        return (
            "builder3: missing source file",
            "a build operation referenced a file that doesn't exist (a missing source or un-generated intermediate) — often a skipped Python pre-build step; check the recipe",
        );
    }
    // fontc requires identical per-master feature (.fea) files
    if low.contains("feature files are non-identical") {
        return (
            "builder3: per-master feature mismatch",
            "the masters' feature (.fea) files are non-identical, which fontc requires to match — reconcile the per-master features upstream",
        );
    }
    // a builder3 build-step failure with no more specific cause above (still better than a bare "builder3 error")
    if low.contains("while building") || low.contains("recipe is not valid") {
        return (
            "builder3: build operation failed",
            "a gftools-builder3 build operation failed with no more specific cause — open the family log",
        );
    }
    // builder3 orchestrator-level error with NO more specific cause above — the last-resort builder3 bucket
    // (must come AFTER the specific fontc/source buckets: every builder3 attempt error is "builder3:"-prefixed,
    // so checking this first would shadow them — especially in orchestrator=builder3 mode where it's the ONLY
    // attempt and EVERY error is builder3-prefixed).
    if low.starts_with("builder3:") {
        return (
            "builder3 error",
            "gftools-builder3 hit an orchestrator-level error with no more specific cause — see the log; in auto mode it then falls back to builder2, in builder3-only mode this is terminal",
        );
    }
    if low.contains("builder") || low.contains("fontmake") || low.contains("fontc") || low.contains("gftools") {
        return ("build error", "the source or build tooling failed — open the family log");
    }
    ("other", "open the family log for details")
}

/// Extract the substring between `a` and the next `b` after it (None if not found).
fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let i = s.find(a)? + a.len();
    let rest = &s[i..];
    let j = rest.find(b)?;
    Some(&rest[..j])
}

/// A finer sub-cause WITHIN a category, for the per-iteration detailed breakdown (the dashboard tallies
/// these per cause). Data-driven keys: the source file type, the recipe operation, the offending config
/// source, the recipe provider, the pre-build tool, the missing designspace field. None when there is no
/// finer signal (the category itself is the whole story). Branch order mirrors `categorize_failure` where
/// messages overlap (e.g. a fontprimer error contains both "recipeprovider" and "could not parse config").
pub fn subclassify_failure(error: &str) -> Option<String> {
    let low = error.to_lowercase();
    // source file type (e.g. ".glyphs", ".vfb") from `for file "…ext"`
    if low.contains("wrong convertor for file") || low.contains("unknown file type for file") {
        if let Some(q) = between(error, "for file \"", "\"") {
            let base = q.rsplit('/').next().unwrap_or(q);
            let ext = base.rsplit('.').next().unwrap_or("");
            return Some(if ext.is_empty() { "source: (no ext)".into() } else { format!("source: .{}", ext.to_lowercase()) });
        }
    }
    // a recipe provider builder3 doesn't implement (must precede the config-parse branch — same message)
    if low.contains("recipeprovider") {
        if let Some(p) = between(&low, "unknown variant `", "`").or_else(|| between(&low, "unknown variant '", "'")) {
            return Some(format!("provider: {}", p));
        }
    }
    // a designspace field the loader doesn't support
    if low.contains("missing field") {
        if let Some(f) = between(error, "missing field `", "`") {
            return Some(format!("missing field: {}", f));
        }
    }
    // which config failed to parse, and why
    if low.contains("could not parse config") || low.contains("did not match any variant") || low.contains("data did not match") {
        let src = if error.contains("__gflib_override_config") { "override config" } else { "family config" };
        let why = if low.contains("untagged enum step") { "unknown recipe Step" } else { "schema mismatch" };
        return Some(format!("{}: {}", src, why));
    }
    // the recipe operation that failed (ToBytes / Fontc / Autohint / …)
    if let Some(op) = between(error, "operation '", "'") {
        return Some(format!("op: {}", op));
    }
    // the tool a refused Python pre-build invokes
    if low.contains("needs python pre-build") {
        if let Some(t) = between(error, "rule uses '", "'") {
            let base = t.split_whitespace().next().unwrap_or(t).rsplit('/').next().unwrap_or(t);
            return Some(format!("tool: {}", base));
        }
    }
    None
}

/// Causes a fresh attempt can plausibly clear — re-attempted automatically on the next build (the
/// self-heal set). Causes needing human action outside the tool (missing -dev lib, unreachable repo,
/// genuine build error) are deliberately excluded; `--retry-failed` re-attempts even those.
const AUTO_RETRY_CATEGORIES: [&str; 10] = [
    "broken dependency venv", "dependency install failed", "transient fetch error",
    "stale archive mirror", "repo not mirrored", "internal/transient I/O",
    "dependency conflict", "build needs setuptools", "misconfigured requirements",
    "build launcher error",
];

pub fn is_auto_retry(cause: &str) -> bool {
    AUTO_RETRY_CATEGORIES.contains(&cause)
}

/// A banner for a family whose displayed result is NOT settled — a fresh attempt is queued, or its
/// failure cause is auto-retried (so recent toolchain/config changes may already address the shown log).
/// Returns None when the result is the current, final outcome. Used by both UIs to avoid presenting a
/// stale failure log as if it were the last word.
pub fn rebuild_pending_note(status: &str, queued_kind: &str, error: &str) -> Option<String> {
    match status {
        "queued" => Some(format!(
            "A fresh {} of this family is queued — the log/error below is from the PREVIOUS attempt.",
            if queued_kind == "rebuild" { "rebuild" } else { "attempt" }
        )),
        "failed" => {
            let (cause, _) = categorize_failure(error);
            if is_auto_retry(cause) {
                Some(format!(
                    "This failure ('{}') is auto-retried on the next run — recent toolchain/config changes may already address the log below, so it can be stale.",
                    cause
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Causes an IMMEDIATE in-build retry can clear (the transient ones). Used by R6 (in-build retry).
#[allow(dead_code)]
const IN_BUILD_RETRY_CATEGORIES: [&str; 2] = ["transient fetch error", "internal/transient I/O"];

#[allow(dead_code)] // wired in R6 (in-build auto-retry of transient failures)
pub fn is_in_build_retry(cause: &str) -> bool {
    IN_BUILD_RETRY_CATEGORIES.contains(&cause)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_python_buckets() {
        assert_eq!(categorize_failure("ModuleNotFoundError: No module named 'gftools'").0, "broken dependency venv");
        assert_eq!(categorize_failure("venv: pip install rc=1 (see c-x.install.log)").0, "dependency install failed");
        assert_eq!(categorize_failure("error: setuptools is required").0, "build needs setuptools");
        assert_eq!(categorize_failure("ResolutionImpossible / dependency conflict").0, "dependency conflict");
        assert_eq!(categorize_failure("resolution too deep").0, "pip resolution too deep");
        assert_eq!(categorize_failure("fontmake: produced no expected font files").0, "output name mismatch");
        assert_eq!(categorize_failure("missing system library: libcairo").0, "missing system library");
        assert_eq!(categorize_failure("not in mirror: deadbeef").0, "stale archive mirror");
        assert_eq!(categorize_failure("kaboom").0, "other");
        // a launcher/exec failure (missing or non-runnable builder binary) — the python_policy=off case
        assert_eq!(categorize_failure(
            "taskset: failed to execute /data/tools/builder3-cf74f20a99/bin/gftools-builder: No such file or directory").0,
            "builder binary missing");
        // ---- the new builder3 R0 buckets (split out of the generic "builder3 error") ----
        assert_eq!(categorize_failure("builder3: recipe is not valid: failed to load source sources/X.glyphs: wrong convertor for file \"sources/X.glyphs\"").0, "builder3: unsupported source format");
        assert_eq!(categorize_failure("builder3: recipe is not valid: could not parse config file sources/config.yaml: recipeprovider: unknown variant `fontprimer`, expected ...").0, "builder3: unsupported recipe provider");
        assert_eq!(categorize_failure("builder3: recipe is not valid: could not parse config file __gflib_override_config.yaml: recipe.fonts[0]: data did not match any variant").0, "builder3: unparseable build config");
        assert_eq!(categorize_failure("builder3: operation X while building [..] from [..]: no such file or directory (os error 2)").0, "builder3: missing source file");
        assert_eq!(categorize_failure("builder3: internal error: entered unreachable code: stat table has no axis values subtable").0, "builder3: internal panic");
        assert_eq!(categorize_failure("builder3: note: run with `RUST_BACKTRACE=1`\nbuilder3: thread 'main' panicked at src/x.rs:9: boom").0, "builder3: internal panic");
        assert_eq!(categorize_failure("builder3: recipe is not valid: failed to load source sources/x.designspace: error parsing designspace file: missing field `conditionset`").0, "builder3: designspace parse error");
        assert_eq!(categorize_failure("builder3: operation X while building [..] from [masters/a]: feature files are non-identical: masters/b").0, "builder3: per-master feature mismatch");
        // a still-generic builder3 error keeps the catch-all
        assert_eq!(categorize_failure("builder3: some brand new orchestrator hiccup").0, "builder3 error");
    }
    #[test]
    fn subclassify_extracts_the_fine_sub_cause() {
        let sc = |e: &str| subclassify_failure(e);
        assert_eq!(sc("builder3: Recipe is not valid: Failed to load source ABeeZee.glyphs: Wrong convertor for file \"ABeeZee.glyphs\"").as_deref(), Some("source: .glyphs"));
        assert_eq!(sc("builder3: Failed to load source X.vfb: Wrong convertor for file \"sources/X.vfb\"").as_deref(), Some("source: .vfb"));
        assert_eq!(sc("builder3: operation 'ToBytes' while building [..]: boom").as_deref(), Some("op: ToBytes"));
        // a fontprimer error contains BOTH "recipeprovider" and "could not parse config" → provider wins
        assert_eq!(sc("builder3: Could not parse config file sources/config.yaml: recipeprovider: unknown variant `fontprimer`").as_deref(), Some("provider: fontprimer"));
        assert_eq!(sc("builder3: Could not parse config file __gflib_override_config.yaml: data did not match any variant of untagged enum Step").as_deref(), Some("override config: unknown recipe Step"));
        assert_eq!(sc("needs Python pre-build: rule uses 'glyphs2ufo' (glyphs2ufo sources/X.glyphspackage)").as_deref(), Some("tool: glyphs2ufo"));
        assert_eq!(sc("builder3: error parsing designspace file: missing field `conditionset`").as_deref(), Some("missing field: conditionset"));
        assert_eq!(sc("builder3: some brand new orchestrator hiccup"), None);
        // …but a fontc error about a missing SOURCE file is NOT the launcher bucket
        assert_ne!(categorize_failure("no such file or directory: sources/Foo.glif").0, "builder binary missing");
    }
    #[test]
    fn fontc_and_source_level_buckets() {
        // Feed the ACTUAL recorded strings: build.rs prefixes every attempt error with the attempt name —
        // "builder3: <msg>" (builder3 attempts, incl. the whole orchestrator=builder3 mode) or "<backend>:
        // <msg>" (e.g. "fontc: <msg>" for builder2+fontc). The specific buckets must win over the generic
        // "builder3:"/"build error" catch-alls regardless of prefix.
        assert_eq!(categorize_failure("fontc: Reading source failed for 'NotoSans-Regular.ufo.json': 'only UFO (directory) packages are supported'").0,
            "fontc: unreadable .ufo.json source");
        // the cyclic-graph error is ALWAYS builder3-prefixed (builder3 is the only orchestrator that emits it)
        assert_eq!(categorize_failure("builder3: cyclic dependency graph (subset inclusion) — not a valid DAG").0,
            "builder3: cyclic source graph");
        // in orchestrator=builder3 mode EVERY error is builder3-prefixed — the specific buckets must still win
        assert_eq!(categorize_failure("builder3: FEA parsing failed with 2 errors, set log level to warn").0,
            "fontc: FEA parse error");
        assert_eq!(categorize_failure("builder3: Missing mapping on Weight for default at DesignSpace(400.0)").0,
            "fontc: axis-mapping gap");
        assert_eq!(categorize_failure("builder3: 'haSquare' has interpolation-incompatible paths").0,
            "interpolation-incompatible masters");
        // builder2+fontc records the same kinds of error with a "fontc:" prefix
        assert_eq!(categorize_failure("fontc: FEA parsing failed with 2 errors").0, "fontc: FEA parse error");
        assert_eq!(categorize_failure("fontc: ValueError: unknown fontmake arg '--subroutinizer'").0,
            "fontc: unsupported fontmake option");
        assert_eq!(categorize_failure("fontc: fontTools.ufoLib.errors.GlifLibError: The file 'ncircumflexbelow.glif' ... does not exist").0,
            "incomplete UFO source (missing .glif)");
        assert_eq!(categorize_failure("fontc: fontmake.errors.FontmakeError: Writing UFO source failed: [Errno 39] Directory not empty").0,
            "fontmake source-prep failed");
        assert_eq!(categorize_failure("fontc: KeyError: 'sources'").0, "malformed build config");
        // a tightened keyerror: an unrelated KeyError with NO config context is NOT "malformed build config"
        assert_ne!(categorize_failure("fontc: KeyError: 'glyphOrder'").0, "malformed build config");
        // a builder3 error with no specific cause falls to the (relocated) generic builder3 bucket, NOT a wrong one
        assert_eq!(categorize_failure("builder3: something inscrutable happened").0, "builder3 error");
        // a bare generic fontc failure with no recognizable cause still falls back to "build error"
        assert_eq!(categorize_failure("fontc: something inscrutable in gftools").0, "build error");
        // none of the new fontc/source buckets are auto-retried (they need source/config fixes)
        for c in ["fontc: unreadable .ufo.json source", "builder3: cyclic source graph",
                  "fontc: FEA parse error", "fontc: axis-mapping gap", "interpolation-incompatible masters",
                  "fontc: unsupported fontmake option", "incomplete UFO source (missing .glif)",
                  "fontmake source-prep failed", "malformed build config"] {
            assert!(!is_auto_retry(c), "{} must not auto-retry", c);
        }
    }
    #[test]
    fn rebuild_pending_note_flags_stale_failures() {
        // a queued family → its shown result is from a prior attempt
        assert!(rebuild_pending_note("queued", "rebuild", "").unwrap().contains("rebuild"));
        assert!(rebuild_pending_note("queued", "retry", "").is_some());
        // a failed family whose cause is auto-retried (e.g. gelasio's dependency conflict) → stale-able
        assert!(rebuild_pending_note("failed", "", "ResolutionImpossible / dependency conflict").is_some());
        assert!(rebuild_pending_note("failed", "", "error: setuptools is required").is_some());
        // a genuine non-auto-retry failure → no banner (the result IS the last word)
        assert!(rebuild_pending_note("failed", "", "fontmake: cannot map glyph to U+0041").is_none());
        // a built family → never a pending banner
        assert!(rebuild_pending_note("built", "", "").is_none());
    }
    #[test]
    fn auto_retry_membership() {
        assert!(is_auto_retry("build needs setuptools"));
        assert!(is_auto_retry("dependency install failed"));
        assert!(!is_auto_retry("missing system library"));
        assert!(!is_auto_retry("build error"));
        assert!(!is_auto_retry("output name mismatch"));
    }
    #[test]
    fn transient_is_specific() {
        assert!(is_transient_clone_error("fatal: early EOF"));
        assert!(is_transient_clone_error("RPC failed; returned error: 503"));
        assert!(!is_transient_clone_error("KeyError 500 in instances")); // not a loose 500 match
    }
}
