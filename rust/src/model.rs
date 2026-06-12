//! Shared data model — the on-disk JSON schema (status.json / state.json / control.json) is kept
//! byte-compatible with the original Python implementation's schema (kept for resumability + external tools): the Rust monitor
//! can render a snapshot written by the Python daemon and vice-versa. Every struct is `#[serde(
//! default)]`-friendly so a snapshot from either side tolerates missing/extra keys across versions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One buildable family: where its source is, the pinned commit, and what it ships.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Family {
    pub slug: String,            // e.g. "ofl/roboto"
    pub name: String,            // display name from METADATA.pb
    pub url: String,             // upstream repository URL
    pub commit: String,          // pinned commit (HEAD when --source archive)
    #[serde(default)]
    pub config_yaml: String,     // path of the gftools-builder config within the repo (may be empty)
    #[serde(default)]
    pub has_config: bool,
    #[serde(default)]
    pub shipped_fonts: Vec<String>, // basenames GF currently ships (for output-name matching/compare)
}

/// Per-family build result — the unit of state persisted in state.json and surfaced in snapshots.
/// `compiler_version` / `builder` / `builder_version` are the M0 provenance axes (compiler vs the
/// build orchestrator), recorded on every attempt whether it succeeds or fails.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Res {
    pub slug: String,
    #[serde(default)]
    pub status: String,          // queued | building | built | failed | skipped
    #[serde(default)]
    pub backend: String,         // fontc | fontmake | both  (the compiler that ran/attempted)
    #[serde(default)]
    pub compiler_version: String, // M0: exact compiler version
    #[serde(default)]
    pub builder: String,         // M0: builder2 | builder3  (the orchestrator)
    #[serde(default)]
    pub builder_version: String, // M0: exact orchestrator version
    #[serde(default)]
    pub python_version: String,  // M0: the Python interpreter that built it (multi-Python ladder)
    #[serde(default)]
    pub cohort: String,
    #[serde(default)]
    pub note: String,            // transient ("checkout", "pre-build", "installing deps")
    #[serde(default)]
    pub queued_kind: String,     // new | retry | rebuild
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub log: String,             // path to the per-family log
    #[serde(default)]
    pub out_bytes: u64,
    #[serde(default)]
    pub out_missing: usize,
    #[serde(default)]
    pub compare: String,         // vs-shipped result (identical / differs / n/a)
    #[serde(default)]
    pub worker: i64,
    #[serde(default)]
    pub started: f64,
    #[serde(default)]
    pub ended: f64,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub timings: BTreeMap<String, f64>, // per-operation seconds for this family (extract/venv/build/…)
    #[serde(default)]
    pub config_sig: String, // content hash of the gflib-build override config (+ build_rules entry) used
                            // for this attempt; lets the daemon auto-rebuild when we change the fix
    #[serde(default)]
    pub upgrade_attempted: String, // toolchain signature (pins + orchestrator) this result was last
                                   // attempted under; a built family below the top rung is auto
                                   // re-attempted ("upgrade") exactly once per signature
}

// ---- snapshot sub-records (mirrors of the dicts Python's snapshot() emits) ----

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Counts {
    #[serde(default)] pub built: usize,
    #[serde(default)] pub failed: usize,
    #[serde(default)] pub building: usize,
    #[serde(default)] pub queued: usize,
    #[serde(default)] pub skipped: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Backends {
    #[serde(default)] pub fontc: usize,
    #[serde(default)] pub fontmake: usize,
    #[serde(default)] pub both: usize, // built with --backend both (fontc AND fontmake compared)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildingItem {
    #[serde(default)] pub slug: String,
    #[serde(default)] pub worker: i64,
    #[serde(default)] pub dur: f64,
    #[serde(default)] pub backend: String,
    #[serde(default)] pub note: String,
    #[serde(default)] pub frozen: bool, // SIGSTOP-frozen by a lowered job limit / pause (UI marks it)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueuedItem {
    #[serde(default)] pub slug: String,
    #[serde(default)] pub kind: String,
    #[serde(default)] pub crater: String, // fontc_crater's verdict token (see crater::CraterStatus::token)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FailItem {
    #[serde(default)] pub slug: String,
    #[serde(default)] pub error: String,
    #[serde(default)] pub log: String,
    #[serde(default)] pub ended: f64,
    #[serde(default)] pub backend: String,
    #[serde(default)] pub compiler_version: String,
    #[serde(default)] pub builder: String,
    #[serde(default)] pub builder_version: String,
    #[serde(default)] pub crater: String, // fontc_crater's verdict token (see crater::CraterStatus::token)
    #[serde(default)] pub rebuild_note: String, // non-empty when a fresh attempt is pending (auto-retry) — the log may be stale
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuiltItem {
    #[serde(default)] pub slug: String,
    #[serde(default)] pub backend: String,
    #[serde(default)] pub bytes: u64,
    #[serde(default)] pub compare: String,
    #[serde(default)] pub log: String,
    #[serde(default)] pub ended: f64,
    #[serde(default)] pub compiler_version: String,
    #[serde(default)] pub builder: String,
    #[serde(default)] pub builder_version: String,
    #[serde(default)] pub python_version: String, // M0: the Python that built it (multi-Python ladder)
    #[serde(default)] pub packaged: bool, // a debian/ packaging tree has been drafted on disk
    #[serde(default)] pub deb_status: String, // "" | built | validated | lint-clean | failed (from build-results.json)
    #[serde(default)] pub deb_lint: String, // lintian outcome: "clean" | "N warnings" | "N errors, M warnings" | "not run (lintian absent)" | ""
    #[serde(default)] pub crater: String, // fontc_crater's verdict token (see crater::CraterStatus::token)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FailCategory {
    #[serde(default)] pub cat: String,
    #[serde(default)] pub count: usize,
    #[serde(default)] pub hint: String,
    #[serde(default)] pub families: Vec<String>,
}

/// One lintian finding tag grouped across packages (the packaging analogue of FailCategory).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LintCategory {
    #[serde(default)] pub tag: String,
    #[serde(default)] pub severity: String, // "E" (error) | "W" (warning)
    #[serde(default)] pub count: usize,      // packages affected
    #[serde(default)] pub families: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CohortView {
    #[serde(default)] pub key: String,
    #[serde(default)] pub count: usize,
    #[serde(default)] pub requirements: String,
    #[serde(default)] pub families: Vec<CohortFam>,
    #[serde(default)] pub cached: bool,
}

/// A cohort member: its display name + current build status (so both UIs can colour it).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CohortFam {
    #[serde(default)] pub name: String,
    #[serde(default)] pub status: String, // built | failed | building | queued | pending
}

// ---- fontspector QA (the --fontspector pass) ----

/// Per-status counts for a fontspector run (PASS/WARN/FAIL/… → count).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FsCounts {
    #[serde(default)] pub pass: usize,
    #[serde(default)] pub warn: usize,
    #[serde(default)] pub fail: usize,
    #[serde(default)] pub fatal: usize,
    #[serde(default)] pub error: usize,
    #[serde(default)] pub skip: usize,
    #[serde(default)] pub info: usize,
}

/// One family's fontspector result, stored on disk (build_dir/fontspector/<slug__>.json) AND
/// surfaced (summary form) in the snapshot for the breakdown panels.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FsFamily {
    #[serde(default)] pub slug: String,
    #[serde(default)] pub counts: FsCounts,
    #[serde(default)] pub worst: String, // the worst status seen (FAIL/WARN/PASS/…) — for sorting/colour
}

/// One check, aggregated across all QA'd families (panel B: a check across families).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FsCheck {
    #[serde(default)] pub id: String,
    #[serde(default)] pub title: String,
    #[serde(default)] pub counts: FsCounts,
    #[serde(default)] pub fail_families: Vec<String>, // families with FAIL/FATAL/ERROR on this check
    #[serde(default)] pub warn_families: Vec<String>, // families with WARN on this check
}

/// The fontspector aggregate carried in the snapshot (the on-disk _summary.json).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FontspectorView {
    #[serde(default)] pub version: String,   // exact fontspector version that ran (saved as metadata)
    #[serde(default)] pub profile: String,
    #[serde(default)] pub ts: f64,           // when the pass last completed
    #[serde(default)] pub families_checked: usize,
    #[serde(default)] pub total: FsCounts,   // grand totals
    #[serde(default)] pub per_check: Vec<FsCheck>,   // panel B
    #[serde(default)] pub per_family: Vec<FsFamily>, // panel A (the family list)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FailHist {
    #[serde(default)] pub ts: f64,
    #[serde(default)] pub slug: String,
    #[serde(default)] pub cause: String,
    #[serde(default)] pub error: String,
    #[serde(default)] pub backend: String,
    #[serde(default)] pub compiler_version: String,
    #[serde(default)] pub builder: String,
    #[serde(default)] pub builder_version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ArchiveRecent {
    #[serde(default)] pub repo: String,
    #[serde(default)] pub status: String,   // added | failed
    #[serde(default)] pub ts: f64,
    #[serde(default)] pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ArchiveView {
    #[serde(default)] pub total: usize,                 // repos in the WHOLE archive on disk
    #[serde(default)] pub active: Vec<String>,          // cloning right now
    #[serde(default)] pub recent: Vec<ArchiveRecent>,   // last 30 min
    #[serde(default)] pub pending: Vec<String>,         // queued next (truncated)
    #[serde(default)] pub pending_total: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResetPortion {
    #[serde(default)] pub key: String,   // control verb value (reset_portion)
    #[serde(default)] pub label: String,
    #[serde(default)] pub bytes: u64,    // current on-disk size (0 = nothing to delete);
                                         // while deleting: the frozen total the bar runs against
    #[serde(default)] pub hint: String,  // what deleting does / what survives
    #[serde(default)] pub deleting: bool, // a deletion is in flight — UIs render the progress bar
    #[serde(default)] pub freed: u64,    // bytes freed so far (live while deleting)
    #[serde(default)] pub note: String,  // transient outcome shown on the row ("✓ freed X" / "⛔ refused …")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskItem {
    #[serde(default)] pub key: String,
    #[serde(default)] pub name: String,
    #[serde(default)] pub status: String,   // done | running | pending | failed | na
    #[serde(default)] pub elapsed: f64,
    #[serde(default)] pub done: usize,
    #[serde(default)] pub total: usize,
    #[serde(default)] pub detail: String,
}

/// Per-operation timing aggregate (bottleneck analysis), matching the Python `op_stats` shape.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpStat {
    #[serde(default)] pub total: f64,
    #[serde(default)] pub count: usize,
    #[serde(default)] pub mean: f64,
    #[serde(default)] pub max: f64,
}

/// The live full snapshot — what both UIs render and the daemon writes to status.json each ~1 s.
/// The fontc_crater comparison summary carried in the snapshot: how our build outcomes line up with
/// fontc_crater's latest verdict, family by family. The headline is `we_build_fontc_cant` — families
/// WE compile that fontc_crater's fontc cannot (the build fixes most valuable to the fontc team).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CraterView {
    #[serde(default)] pub run: String,         // crater latest-run date
    #[serde(default)] pub fontc_rev: String,   // fontc revision crater ran
    #[serde(default)] pub fonts_repo_sha: String, // google/fonts SHA crater compared against
    #[serde(default)] pub complete: bool,       // false = diff-only fallback (no fontc/both-failed split)
    #[serde(default)] pub matched: usize,       // families joined to a crater verdict
    // our build status × crater verdict (the actionable buckets):
    #[serde(default)] pub we_build_fontc_cant: usize, // built here, fontc failed in crater — GOLD
    #[serde(default)] pub we_fail_fontc_ok: usize,    // failed here, fontc built it — our regressions
    #[serde(default)] pub both_ok_identical: usize,
    #[serde(default)] pub both_ok_diff: usize,
    #[serde(default)] pub gold_families: Vec<String>, // capped slug list for the panel
    #[serde(default)] pub regression_families: Vec<String>, // capped slug list for the panel
    // raw crater verdict tally across matched families:
    #[serde(default)] pub c_identical: usize,
    #[serde(default)] pub c_diff: usize,
    #[serde(default)] pub c_fontc_failed: usize,
    #[serde(default)] pub c_fontmake_failed: usize,
    #[serde(default)] pub c_both_failed: usize,
    #[serde(default)] pub c_repo_failed: usize,
}

/// One build-tool package (a dependency, compiler, or orchestrator) + the families that need it,
/// classified python/rust — the per-tool Python->Rust (M5) burn-down view.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolPkg {
    #[serde(default)] pub name: String,
    #[serde(default)] pub lang: String,             // "python" | "rust"
    #[serde(default)] pub kind: String,             // "requirement" | "compiler" | "orchestrator"
    #[serde(default)] pub families: usize,          // how many families build-depend on it
    #[serde(default)] pub family_list: Vec<String>, // capped slug list (for the detail overlay)
    #[serde(default)] pub packaged: bool,           // a .deb has been built for this tool (none yet)
}

/// A required external program for deb building/validation, and whether it is currently on PATH.
/// Re-detected periodically so the UI recovers as soon as a missing tool is installed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DebTool {
    #[serde(default)] pub name: String,     // program (e.g. "lintian")
    #[serde(default)] pub present: bool,
    #[serde(default)] pub provides: String, // apt package that provides it
    #[serde(default)] pub purpose: String,
}

/// Defaulted everywhere so a partial/foreign status.json still deserializes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)] pub elapsed: f64,
    #[serde(default)] pub disk_used_delta: u64,
    #[serde(default)] pub disk_free: u64,
    #[serde(default)] pub disk_build_total: u64,
    #[serde(default)] pub disk_archive_total: u64,
    #[serde(default)] pub disk_archive_nested: bool,
    #[serde(default)] pub jobs: usize,
    #[serde(default)] pub paused: bool,
    #[serde(default)] pub total: usize,
    #[serde(default)] pub counts: Counts,
    #[serde(default)] pub backends: Backends,
    #[serde(default)] pub building: Vec<BuildingItem>,
    #[serde(default)] pub running_builds: usize, // in-flight builder children (frozen by a pause)
    #[serde(default)] pub frozen_builds: usize,  // of those, how many are SIGSTOP-frozen (pause or job limit)
    #[serde(default)] pub failures_recent: Vec<FailItem>,
    #[serde(default)] pub built_recent: Vec<BuiltItem>,
    #[serde(default)] pub packages: Vec<BuiltItem>, // ALL built families (uncapped) for the packaging tab
    #[serde(default)] pub queued_list: Vec<QueuedItem>,
    #[serde(default)] pub fail_categories: Vec<FailCategory>,
    #[serde(default)] pub lint_categories: Vec<LintCategory>, // lintian findings grouped by tag (packaging)
    // packaging/lint queue (the .deb pipeline, mirroring the main build queue)
    #[serde(default)] pub build_debs: bool,      // the .deb packaging SETTING is active (gates the packaging bar)
    #[serde(default)] pub pkg_now: String,      // current package-worker activity ("packaging/linting <slug>" or "")
    #[serde(default)] pub pkg_pending: usize,    // built families not yet packaged (.deb to build)
    #[serde(default)] pub lint_total: usize,     // packages with a .deb (lintable)
    #[serde(default)] pub lint_done: usize,      // of those, how many lintian has already run on
    #[serde(default)] pub lint_pending: usize,   // lintable packages still awaiting lintian
    #[serde(default)] pub cohorts: Vec<CohortView>,
    #[serde(default)] pub cohorts_ready: usize,
    #[serde(default)] pub tool_packages: Vec<ToolPkg>, // build-tool packages + their dependent families
    #[serde(default)] pub deb_tools: Vec<DebTool>,     // required deb-build external programs + availability
    #[serde(default)] pub phase: String,
    #[serde(default)] pub phase_total: usize,
    #[serde(default)] pub phase_done: usize,
    #[serde(default)] pub phase_label: String,
    #[serde(default)] pub phase_error: String,
    #[serde(default)] pub failure_history: Vec<FailHist>,
    #[serde(default)] pub tooling: BTreeMap<String, String>,   // M0: compiler -> version
    #[serde(default)] pub builders: BTreeMap<String, String>,  // M0: builder2/builder3 -> version
    #[serde(default)] pub migration: BTreeMap<String, usize>,
    #[serde(default)] pub python_versions: BTreeMap<String, usize>, // M0: built families per Python interpreter
    #[serde(default)] pub op_stats: BTreeMap<String, OpStat>,       // per-op timing (stats tab)
    #[serde(default)] pub phase_durations: BTreeMap<String, f64>,
    #[serde(default)] pub tasks: Vec<TaskItem>,
    #[serde(default)] pub reset_portions: Vec<ResetPortion>, // the reset tab's deletable portions + sizes

    #[serde(default)] pub archive_recent: Vec<ArchiveRecent>,
    #[serde(default)] pub archive: ArchiveView,
    #[serde(default)] pub config: BTreeMap<String, serde_json::Value>,
    #[serde(default)] pub control_log: Vec<String>,
    #[serde(default)] pub dep_relaxations: Vec<String>, // auto-relaxed pins / forced overrides (R2)
    #[serde(default)] pub config_path: String,
    #[serde(default)] pub pre_build: bool, // first-run setup wizard (config tab is the only view)
    #[serde(default)] pub fontspector: Option<FontspectorView>, // QA results (the --fontspector pass)
    #[serde(default)] pub crater: Option<CraterView>, // fontc_crater build-status comparison (when loaded)
    #[serde(default)] pub done: bool,
    #[serde(default)] pub daemon_alive: bool,
}

/// The full `state.json` document — byte-compatible with the Python tool so resume preserves the
/// cohort map, the cumulative clock, and the per-family results across a Python→Rust migration (R1).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StateFile {
    #[serde(default)] pub saved_at: f64,
    #[serde(default)] pub build_dir: String,
    #[serde(default)] pub elapsed_so_far: f64,
    #[serde(default)] pub results: BTreeMap<String, Res>,
    #[serde(default)] pub cohort_members: BTreeMap<String, Vec<String>>,
    #[serde(default)] pub cohort_reqs: BTreeMap<String, String>,
}

/// A live-control message dropped into control.json by a monitor; the daemon applies it on the fly.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Control {
    #[serde(default)] pub seq: u64,
    #[serde(default)] pub set: ControlSet,
}

/// Only the keys actually being set are serialized (unset = omitted, never `null`) so a control.json
/// the Rust UI writes is byte-identical to one the Python tool writes — a Python daemon reading it
/// won't trip over a `null` percent/jobs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ControlSet {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub jobs: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub compare: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub retry: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub retry_all: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub retry_overrides: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub repackage_all: Option<bool>, // rebuild every .deb from existing fonts (apply packaging fixes + re-lint)
    #[serde(default, skip_serializing_if = "Option::is_none")] pub build_debs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub restart: Option<bool>, // re-exec the daemon (apply startup-only settings)
    // delete one resettable portion of the build system (reset tab): fonts-fontc | fonts-fontmake |
    // variants | debs | venvs | pip-cache | logs | work | fontspector | tools
    #[serde(default, skip_serializing_if = "Option::is_none")] pub reset_portion: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_roundtrips() {
        let mut s = Snapshot::default();
        s.jobs = 8;
        s.counts.built = 3;
        s.tooling.insert("fontc".into(), "fontc 0.9 (git abc)".into());
        s.builders.insert("builder2".into(), "gftools-builder2 0.9.74".into());
        s.built_recent.push(BuiltItem {
            slug: "ofl/x".into(),
            builder_version: "gftools-builder2 0.9.74".into(),
            ..Default::default()
        });
        let txt = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&txt).unwrap();
        assert_eq!(back.jobs, 8);
        assert_eq!(back.counts.built, 3);
        assert_eq!(back.builders.get("builder2").unwrap(), "gftools-builder2 0.9.74");
        assert_eq!(back.built_recent[0].builder_version, "gftools-builder2 0.9.74");
    }
    #[test]
    fn tolerates_partial_foreign_json() {
        // a status.json from the Python tool with only a few keys must still deserialize
        let txt = r#"{"jobs":4,"counts":{"failed":2},"phase":"build","extra_unknown_key":123}"#;
        let s: Snapshot = serde_json::from_str(txt).unwrap();
        assert_eq!(s.jobs, 4);
        assert_eq!(s.counts.failed, 2);
        assert_eq!(s.phase, "build");
    }
    #[test]
    fn control_set_roundtrips() {
        let txt = r#"{"jobs":6,"retry":["ofl/x"],"paused":true}"#;
        let cs: ControlSet = serde_json::from_str(txt).unwrap();
        assert_eq!(cs.jobs, Some(6));
        assert_eq!(cs.retry, Some(vec!["ofl/x".to_string()]));
        assert_eq!(cs.paused, Some(true));
    }
    #[test]
    fn control_set_retry_overrides() {
        // the "retry config-fixed" UI button posts exactly this
        let cs: ControlSet = serde_json::from_str(r#"{"retry_overrides":true}"#).unwrap();
        assert_eq!(cs.retry_overrides, Some(true));
        // and an unset retry_overrides is omitted (not null) so the control.json stays byte-compatible
        let cs2 = ControlSet { retry: Some(vec!["ofl/y".into()]), ..Default::default() };
        assert!(!serde_json::to_string(&cs2).unwrap().contains("retry_overrides"));
        assert!(!serde_json::to_string(&cs2).unwrap().contains("restart"));
        // the web "Restart daemon" button posts exactly this
        assert_eq!(serde_json::from_str::<ControlSet>(r#"{"restart":true}"#).unwrap().restart, Some(true));
    }
}
