//! fontc_crater comparison — load fontc_crater's latest per-target verdict and join it to our
//! families by upstream repo, so each family can show what fontc_crater concluded (identical / diff /
//! fontc-failed / fontmake-failed / both-failed) right next to our own build result.
//!
//! The strategic payload (why this matters most to the fontc team): a family WE build successfully
//! but that fontc_crater's *fontc* cannot compile is exactly a build fix worth upstreaming — and the
//! very same config.yaml / build rule that unblocks our Debian packaging unblocks fontc_crater too,
//! because both resolve `sources` against the repo root identically.
//!
//! Data source: `fontc_crater_targets.json` (the complete per-target export written by gfonts_agents'
//! `fetch_crater_analysis.py`). If only the older diff-focused `fontc_crater_analysis.json` is present
//! we still load it as a PARTIAL fallback (diffs + source-missing repos only — the fontc/both-failed
//! split is absent there), and flag `complete=false` so the UI can say so.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// fontc_crater's verdict for one repo, aggregated across that repo's targets.
#[derive(Clone, Debug, PartialEq)]
pub enum CraterStatus {
    Identical,      // fontc and fontmake produced byte-identical output
    Diff(f32),      // both compiled; outputs differ (similarity 0..1, worst across the repo's targets)
    FontcFailed,    // fontc failed to compile (fontmake succeeded)
    FontmakeFailed, // fontmake failed to compile (fontc succeeded)
    BothFailed,     // neither compiler produced output
    RepoFailed,     // crater could not even produce a target (missing source / clone) — fontc never ran
}

impl CraterStatus {
    /// Severity ordering for aggregating a repo's several targets into one verdict (worst wins).
    fn rank(&self) -> u8 {
        match self {
            CraterStatus::BothFailed => 6,
            CraterStatus::RepoFailed => 5,
            CraterStatus::FontcFailed => 4,
            CraterStatus::FontmakeFailed => 3,
            CraterStatus::Diff(_) => 2,
            CraterStatus::Identical => 1,
        }
    }

    /// Did fontc manage to compile this in crater? (drives the "we build / fontc can't" highlight)
    pub fn fontc_built(&self) -> bool {
        matches!(
            self,
            CraterStatus::Identical | CraterStatus::Diff(_) | CraterStatus::FontmakeFailed
        )
    }

    /// fontc could NOT compile this (the gold case when WE can): includes the repo-level failures
    /// where crater's target config is itself broken — our fixed config.yaml is what would unblock it.
    pub fn fontc_failed(&self) -> bool {
        matches!(
            self,
            CraterStatus::FontcFailed | CraterStatus::BothFailed | CraterStatus::RepoFailed
        )
    }

    /// A short, fixed-width-friendly token for the UI column (no leading "crater:" — the header says so).
    pub fn token(&self) -> String {
        match self {
            CraterStatus::Identical => "match".into(),
            CraterStatus::Diff(s) => format!("~{}%", (s * 100.0).round() as i32),
            CraterStatus::FontcFailed => "fontc-fail".into(),
            CraterStatus::FontmakeFailed => "fmake-fail".into(),
            CraterStatus::BothFailed => "both-fail".into(),
            CraterStatus::RepoFailed => "src-miss".into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CraterMeta {
    pub latest_run: String,
    pub fontc_rev: String,
    pub fonts_repo_sha: String,
    pub complete: bool, // true = full per-target file; false = analysis fallback (diffs only)
}

/// The loaded crater verdict, keyed by normalized "owner/repo" (lower-cased) — the join key against
/// a family's upstream repository_url via [`crate::build::repo_slug`].
#[derive(Clone, Debug, Default)]
pub struct CraterData {
    pub meta: CraterMeta,
    pub by_repo: BTreeMap<String, CraterStatus>,
}

impl CraterData {
    /// Crater's verdict for a family given its upstream repository URL (None if crater never saw it).
    pub fn status_for_url(&self, url: &str) -> Option<&CraterStatus> {
        let key = crate::build::repo_slug(url).to_lowercase();
        self.by_repo.get(&key)
    }
}

/// Resolve the crater file: explicit override first, then a small search of the usual spots
/// (gflib-data, then the sibling gfonts_agents dashboard data dir). Prefers the complete per-target
/// file over the diff-only analysis file. Returns the first existing path, or None.
pub fn resolve_path(explicit: Option<&Path>, data_dir: &Path) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return if p.exists() { Some(p.to_path_buf()) } else { None };
    }
    let candidates = [
        data_dir.join("fontc_crater_targets.json"),
        data_dir.join("fontc_crater_analysis.json"),
        PathBuf::from("../gfonts_agents/data/fontc_crater_targets.json"),
        PathBuf::from("../gfonts_agents/data/fontc_crater_analysis.json"),
        data_dir.join("../../gfonts_agents/data/fontc_crater_targets.json"),
        data_dir.join("../../gfonts_agents/data/fontc_crater_analysis.json"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Normalize a crater target id ("owner/repo/config src?sha (type)") to "owner/repo" (lower-cased).
fn repo_from_target_id(id: &str) -> Option<String> {
    let head = id.split_whitespace().next().unwrap_or(id);
    let mut it = head.split('/');
    let owner = it.next()?;
    let repo = it.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{}/{}", owner, repo).to_lowercase())
}

fn status_from_str(s: &str, sim: Option<f32>) -> Option<CraterStatus> {
    match s {
        "identical" => Some(CraterStatus::Identical),
        "diff" => Some(CraterStatus::Diff(sim.unwrap_or(1.0))),
        "fontc_failed" => Some(CraterStatus::FontcFailed),
        "fontmake_failed" => Some(CraterStatus::FontmakeFailed),
        "both_failed" => Some(CraterStatus::BothFailed),
        "repo_failed" => Some(CraterStatus::RepoFailed),
        _ => None,
    }
}

/// Fold one more target verdict into a repo's running aggregate. We prefer the `gftools` build type
/// (apples-to-apples with what we and google/fonts ship via gftools-builder): once any gftools verdict
/// is seen, non-gftools verdicts are ignored. Within the chosen build type, the worst verdict wins,
/// and Diff keeps the lowest similarity.
fn fold(acc: &mut BTreeMap<String, (bool, CraterStatus)>, repo: String, is_gftools: bool, st: CraterStatus) {
    match acc.get_mut(&repo) {
        None => {
            acc.insert(repo, (is_gftools, st));
        }
        Some((had_gftools, cur)) => {
            // a gftools verdict supersedes any non-gftools one already recorded
            if is_gftools && !*had_gftools {
                *had_gftools = true;
                *cur = st;
                return;
            }
            // ignore a non-gftools verdict once we've locked onto gftools
            if *had_gftools && !is_gftools {
                return;
            }
            // same build-type class: keep the worst; for two Diffs keep the lower similarity
            let take = match (&*cur, &st) {
                (CraterStatus::Diff(a), CraterStatus::Diff(b)) => {
                    if b < a { st } else { return; }
                }
                _ if st.rank() > cur.rank() => st,
                _ => return,
            };
            *cur = take;
        }
    }
}

/// Load and parse a crater file (either format). Returns None if absent/unparseable.
pub fn load(path: &Path) -> Option<CraterData> {
    let txt = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let meta_obj = v.get("_metadata");
    let mut meta = CraterMeta {
        latest_run: meta_obj.and_then(|m| m.get("latest_run")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
        fontc_rev: meta_obj.and_then(|m| m.get("fontc_rev")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
        fonts_repo_sha: meta_obj.and_then(|m| m.get("fonts_repo_sha")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
        complete: false,
    };

    let targets = v.get("targets").and_then(|t| t.as_array());
    let is_complete = targets
        .and_then(|a| a.first())
        .map(|first| first.get("status").is_some())
        .unwrap_or(false);

    let mut acc: BTreeMap<String, (bool, CraterStatus)> = BTreeMap::new();

    if is_complete {
        meta.complete = true;
        for t in targets.unwrap() {
            let status_s = t.get("status").and_then(|x| x.as_str()).unwrap_or("");
            let sim = t.get("similarity").and_then(|x| x.as_f64()).map(|f| f as f32);
            let st = match status_from_str(status_s, sim) {
                Some(s) => s,
                None => continue,
            };
            // prefer the explicit repo field; fall back to parsing the id
            let repo = t
                .get("repo")
                .and_then(|x| x.as_str())
                .map(|s| s.to_lowercase())
                .or_else(|| t.get("id").and_then(|x| x.as_str()).and_then(repo_from_target_id));
            let repo = match repo {
                Some(r) if !r.is_empty() => r,
                _ => continue,
            };
            let is_gftools = t.get("build_type").and_then(|x| x.as_str()) == Some("gftools");
            fold(&mut acc, repo, is_gftools, st);
        }
    } else {
        // PARTIAL fallback from the diff-focused analysis file: only diffs and source-missing repos
        // are recoverable here (the fontc/fontmake/both compile-failure split lives only in the raw
        // per-target results that the complete file carries).
        meta.complete = false;
        if let Some(arr) = targets {
            for t in arr {
                let id = match t.get("id").and_then(|x| x.as_str()) {
                    Some(s) => s,
                    None => continue,
                };
                let repo = match repo_from_target_id(id) {
                    Some(r) => r,
                    None => continue,
                };
                let sim = t.get("total_similarity").and_then(|x| x.as_f64()).map(|f| f as f32);
                fold(&mut acc, repo, false, CraterStatus::Diff(sim.unwrap_or(1.0)));
            }
        }
        if let Some(fri) = v.get("failed_repos_index").and_then(|x| x.as_object()) {
            for key in fri.keys() {
                // keys are already normalized "owner/repo" (lower-cased)
                fold(&mut acc, key.to_lowercase(), false, CraterStatus::RepoFailed);
            }
        }
    }

    let by_repo = acc.into_iter().map(|(k, (_, st))| (k, st)).collect();
    Some(CraterData { meta, by_repo })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(json: &str) -> CraterData {
        let dir = std::env::temp_dir().join(format!("gflib-crater-{}-{}", std::process::id(), json.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.json");
        std::fs::write(&p, json).unwrap();
        let d = load(&p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        d
    }

    #[test]
    fn complete_format_classifies_and_joins_by_repo() {
        let d = data(r#"{
            "_metadata": {"latest_run":"2026-04-27","fontc_rev":"abc","fonts_repo_sha":"def"},
            "targets": [
                {"id":"Foo/Bar/sources/config.yaml Bar.glyphs?1 (gftools)","repo":"foo/bar","build_type":"gftools","status":"fontc_failed","similarity":null},
                {"id":"Baz/Qux/sources/config.yaml Qux.glyphs?2 (gftools)","repo":"baz/qux","build_type":"gftools","status":"diff","similarity":0.97}
            ]
        }"#);
        assert!(d.meta.complete);
        assert_eq!(d.status_for_url("https://github.com/Foo/Bar"), Some(&CraterStatus::FontcFailed));
        assert_eq!(d.status_for_url("https://github.com/baz/qux.git"), Some(&CraterStatus::Diff(0.97)));
        assert_eq!(d.status_for_url("https://github.com/no/such"), None);
    }

    #[test]
    fn gftools_build_type_supersedes_default() {
        // default says fontc_failed, gftools says identical → the gftools verdict (what GF ships) wins
        let d = data(r#"{
            "_metadata": {},
            "targets": [
                {"id":"o/r/c default","repo":"o/r","build_type":"default","status":"fontc_failed","similarity":null},
                {"id":"o/r/c gftools","repo":"o/r","build_type":"gftools","status":"identical","similarity":1.0}
            ]
        }"#);
        assert_eq!(d.by_repo.get("o/r"), Some(&CraterStatus::Identical));
    }

    #[test]
    fn worst_verdict_wins_within_same_build_type() {
        let d = data(r#"{
            "_metadata": {},
            "targets": [
                {"id":"o/r/a gftools","repo":"o/r","build_type":"gftools","status":"diff","similarity":0.9},
                {"id":"o/r/b gftools","repo":"o/r","build_type":"gftools","status":"both_failed","similarity":null}
            ]
        }"#);
        assert_eq!(d.by_repo.get("o/r"), Some(&CraterStatus::BothFailed));
    }

    #[test]
    fn fontc_failed_and_built_predicates() {
        assert!(CraterStatus::FontcFailed.fontc_failed());
        assert!(CraterStatus::BothFailed.fontc_failed());
        assert!(CraterStatus::RepoFailed.fontc_failed());
        assert!(CraterStatus::Identical.fontc_built());
        assert!(CraterStatus::Diff(0.5).fontc_built());
        assert!(CraterStatus::FontmakeFailed.fontc_built());
        assert!(!CraterStatus::FontmakeFailed.fontc_failed());
    }

    #[test]
    fn partial_analysis_fallback() {
        // the diff-focused analysis file: diffs + failed_repos_index, no per-target status field
        let d = data(r#"{
            "_metadata": {"latest_run":"2026-04-27"},
            "targets": [
                {"id":"Foo/Bar/sources/config.yaml Bar.glyphs?1 (gftools)","total_similarity":0.8,"tables":{}}
            ],
            "failed_repos_index": {"bornaiz/lalezar": {"url":"https://github.com/BornaIz/Lalezar","reason":"missing source"}}
        }"#);
        assert!(!d.meta.complete);
        assert_eq!(d.status_for_url("https://github.com/Foo/Bar"), Some(&CraterStatus::Diff(0.8)));
        assert_eq!(d.status_for_url("https://github.com/BornaIz/Lalezar"), Some(&CraterStatus::RepoFailed));
    }

    #[test]
    fn token_strings() {
        assert_eq!(CraterStatus::Identical.token(), "match");
        assert_eq!(CraterStatus::Diff(0.97).token(), "~97%");
        assert_eq!(CraterStatus::FontcFailed.token(), "fontc-fail");
        assert_eq!(CraterStatus::BothFailed.token(), "both-fail");
    }
}
