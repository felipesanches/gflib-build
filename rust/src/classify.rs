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
    if low.contains("builder") || low.contains("fontmake") || low.contains("fontc") || low.contains("gftools") {
        return ("build error", "the source or build tooling failed — open the family log");
    }
    ("other", "open the family log for details")
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
