//! Worklist discovery — the two first-class sources, ported from the Python `discover` /
//! `discover_from_archive`:
//!   * metadata: parse each `<license>/<family>/METADATA.pb` in a google/fonts clone; a family is
//!     buildable iff it has a pinned commit AND a build config (in-repo `config_yaml` or a local
//!     `config.yaml` override).
//!   * archive: every bare mirror in the archive at `--archive-rev` (HEAD = default-branch tip).

use crate::model::Family;
use std::path::Path;
use std::process::Command;

pub const LICENSE_DIRS: [&str; 3] = ["ofl", "ufl", "apache"];

/// Extract the first capture of a simple `key:\s*"value"` field. We avoid a regex-crate dependency
/// and parse METADATA.pb (a protobuf-text file) with targeted string scans — the same fields the
/// Python regexes capture (repository_url / commit / config_yaml / name / filename).
fn field(txt: &str, key: &str) -> Option<String> {
    // find `key:` then the next quoted string on that logical span
    let mut search = txt;
    while let Some(pos) = search.find(key) {
        let after = &search[pos + key.len()..];
        // require it's a field token: next non-space char is ':'
        let trimmed = after.trim_start();
        if let Some(rest) = trimmed.strip_prefix(':') {
            if let Some(q1) = rest.find('"') {
                if let Some(q2) = rest[q1 + 1..].find('"') {
                    return Some(rest[q1 + 1..q1 + 1 + q2].to_string());
                }
            }
        }
        search = &search[pos + key.len()..];
    }
    None
}

fn all_filenames(txt: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut search = txt;
    while let Some(pos) = search.find("filename:") {
        let rest = &search[pos + "filename:".len()..];
        if let Some(q1) = rest.find('"') {
            if let Some(q2) = rest[q1 + 1..].find('"') {
                out.push(rest[q1 + 1..q1 + 1 + q2].to_string());
                search = &rest[q1 + 1 + q2..];
                continue;
            }
        }
        break;
    }
    out
}

/// Parse one METADATA.pb. Returns None when it has no `source {` block or no repository_url.
fn parse_metadata(meta: &Path) -> Option<(String, String, String, String, Vec<String>)> {
    let txt = std::fs::read_to_string(meta).ok()?;
    if !txt.contains("source {") && !txt.contains("source{") {
        return None;
    }
    let repo = field(&txt, "repository_url")?;
    // commit must look like a hex sha (7-40); field() returns the raw quoted value
    let commit = field(&txt, "commit").unwrap_or_default();
    let commit = if commit.len() >= 7
        && commit.len() <= 40
        && commit.chars().all(|c| c.is_ascii_hexdigit())
    {
        commit
    } else {
        String::new()
    };
    let cfg = field(&txt, "config_yaml").unwrap_or_default();
    let name = field(&txt, "name").unwrap_or_else(|| {
        meta.parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    Some((name, repo, commit, cfg, all_filenames(&txt)))
}

/// Discover buildable families from a google/fonts clone. Returns (families, library_total, skipped).
pub fn discover_metadata(google_fonts: &Path) -> (Vec<Family>, usize, usize) {
    let mut fams = Vec::new();
    let mut library_total = 0usize;
    for lic in LICENSE_DIRS {
        let base = google_fonts.join(lic);
        if !base.is_dir() {
            continue;
        }
        let mut dirs: Vec<_> = match std::fs::read_dir(&base) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(_) => continue,
        };
        dirs.sort();
        for dir in dirs {
            let meta = dir.join("METADATA.pb");
            if !meta.is_file() {
                continue;
            }
            library_total += 1;
            let parsed = match parse_metadata(&meta) {
                Some(p) => p,
                None => continue,
            };
            let (name, repo, commit, cfg, fonts) = parsed;
            let famname = dir.file_name().unwrap().to_string_lossy().to_string();
            let slug = format!("{}/{}", lic, famname);
            let has_override = google_fonts.join(&slug).join("config.yaml").is_file();
            if commit.is_empty() || (!has_override && cfg.is_empty()) {
                continue; // not buildable: no pinned commit, or no config at all
            }
            fams.push(Family {
                slug,
                name,
                url: repo,
                commit,
                config_yaml: cfg,
                has_config: has_override,
                shipped_fonts: fonts,
            });
        }
    }
    let n = fams.len();
    (fams, library_total, library_total - n)
}

/// Discover from the archive: every `<owner>/<repo>.git` mirror, resolved at `rev`. Resolution runs
/// a couple of `git` subprocesses per mirror, so it is parallelised across `jobs` threads (the
/// archive holds ~1300 mirrors). When `want` is set (an `--only` run) we resolve ONLY those slugs —
/// no point running 1300 `git rev-parse`s to build one family.
pub fn discover_archive(
    archive: &Path,
    rev: &str,
    jobs: usize,
    want: Option<&std::collections::HashSet<String>>,
) -> (Vec<Family>, usize, usize) {
    let mut mirrors = Vec::new();
    if let Ok(owners) = std::fs::read_dir(archive) {
        for owner in owners.flatten() {
            if !owner.path().is_dir() {
                continue;
            }
            if let Ok(repos) = std::fs::read_dir(owner.path()) {
                for repo in repos.flatten() {
                    let p = repo.path();
                    if p.extension().map(|e| e == "git").unwrap_or(false) {
                        mirrors.push(p);
                    }
                }
            }
        }
    }
    mirrors.sort();
    let total = mirrors.len();
    if let Some(w) = want {
        mirrors.retain(|m| w.contains(&mirror_slug(m)));
    }

    let mirrors = std::sync::Arc::new(mirrors);
    let rev = rev.to_string();
    let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Family>::new()));
    let nthreads = jobs.clamp(1, 16).min(mirrors.len().max(1));
    let mut handles = Vec::new();
    for _ in 0..nthreads {
        let mirrors = std::sync::Arc::clone(&mirrors);
        let next = std::sync::Arc::clone(&next);
        let out = std::sync::Arc::clone(&out);
        let rev = rev.clone();
        handles.push(std::thread::spawn(move || loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i >= mirrors.len() {
                break;
            }
            if let Some(f) = resolve_mirror(&mirrors[i], &rev) {
                out.lock().unwrap().push(f);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let mut fams = std::sync::Arc::try_unwrap(out).unwrap().into_inner().unwrap();
    fams.sort_by(|a, b| a.slug.cmp(&b.slug));
    let n = fams.len();
    (fams, total, total - n)
}

fn mirror_slug(mirror: &Path) -> String {
    let owner = mirror
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let repo = mirror
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    format!("{}/{}", owner, repo)
}

fn resolve_mirror(mirror: &Path, rev: &str) -> Option<Family> {
    let owner = mirror
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let repo = mirror
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let out = Command::new("git")
        .args([
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", rev),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let url = Command::new("git")
        .args([
            "--git-dir",
            &mirror.to_string_lossy(),
            "config",
            "--get",
            "remote.origin.url",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("https://github.com/{}/{}", owner, repo));
    Some(Family {
        slug: format!("{}/{}", owner, repo),
        name: repo,
        url,
        commit: sha,
        config_yaml: String::new(),
        has_config: false,
        shipped_fonts: Vec::new(),
    })
}

/// Evenly-spaced deterministic sample of `percent`% across the alphabetical list (ported 1:1).
pub fn sample_evenly(items: Vec<Family>, percent: f64) -> Vec<Family> {
    if percent >= 100.0 || items.is_empty() {
        return items;
    }
    let n = items.len();
    let k = ((n as f64 * percent / 100.0).ceil() as usize).max(1);
    if k >= n {
        return items;
    }
    let stride = n as f64 / k as f64;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for i in 0..k {
        let idx = ((i as f64 * stride) as usize).min(n - 1);
        let f = &items[idx];
        if seen.insert(f.slug.clone()) {
            out.push(f.clone());
        }
    }
    out
}

/// Best-effort auto-detect of a fontc binary (PATH, then common checkout locations).
pub fn detect_fontc() -> Option<String> {
    if let Ok(o) = Command::new("sh").args(["-c", "command -v fontc"]).output() {
        let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !p.is_empty() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let cands = [
        "fontc/target/release/fontc".to_string(),
        format!("{}/fontc/target/release/fontc", home),
        "../fontc/target/release/fontc".to_string(),
    ];
    for c in cands {
        if Path::new(&c).is_file() {
            return std::fs::canonicalize(&c)
                .ok()
                .map(|p| p.to_string_lossy().to_string());
        }
    }
    None
}

/// Best-effort auto-detect of a pre-existing repo archive (a dir of {owner}/{repo}.git).
pub fn detect_archive(data_dir: &Path) -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let cands = [
        data_dir.join("archive"),
        Path::new("repo_archive").to_path_buf(),
        Path::new("archive").to_path_buf(),
        Path::new(&home).join("repo_archive"),
        Path::new(&home).join("upstream_repos").join("repo_archive"),
    ];
    for c in cands {
        if c.is_dir() {
            // has at least one */*.git ?
            if let Ok(owners) = std::fs::read_dir(&c) {
                for owner in owners.flatten() {
                    if owner.path().is_dir() {
                        if let Ok(repos) = std::fs::read_dir(owner.path()) {
                            for r in repos.flatten() {
                                if r.path().extension().map(|e| e == "git").unwrap_or(false) {
                                    return std::fs::canonicalize(&c)
                                        .ok()
                                        .map(|p| p.to_string_lossy().to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fam(slug: &str) -> Family {
        Family { slug: slug.into(), ..Default::default() }
    }
    #[test]
    fn sample_is_deterministic_and_spread() {
        let items: Vec<Family> = (0..100).map(|i| fam(&format!("ofl/f{:03}", i))).collect();
        let a = sample_evenly(items.clone(), 10.0);
        let b = sample_evenly(items.clone(), 10.0);
        assert_eq!(a.len(), 10);
        assert_eq!(
            a.iter().map(|f| f.slug.clone()).collect::<Vec<_>>(),
            b.iter().map(|f| f.slug.clone()).collect::<Vec<_>>()
        );
        // spread across the list, not clustered: last pick is far from the first
        assert!(a.last().unwrap().slug > a[0].slug);
    }
    #[test]
    fn full_percent_keeps_everything() {
        let items: Vec<Family> = (0..5).map(|i| fam(&format!("a/{}", i))).collect();
        assert_eq!(sample_evenly(items.clone(), 100.0).len(), 5);
    }
    #[test]
    fn field_parses_quoted_value() {
        let txt = "name: \"Roboto\"\nrepository_url: \"https://x/y\"\n";
        assert_eq!(field(txt, "name"), Some("Roboto".into()));
        assert_eq!(field(txt, "repository_url"), Some("https://x/y".into()));
        assert_eq!(field(txt, "absent"), None);
    }
}
