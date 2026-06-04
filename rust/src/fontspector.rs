//! The optional fontspector QA pass (`--fontspector`). A SEPARATE pass (not part of the build loop):
//! it runs a PINNED fontspector release over every successfully-built family's fonts, records the
//! exact fontspector version as metadata per family, and writes:
//!   build_dir/fontspector/<slug__>.json   — one family's full result {version, profile, ts, checks}
//!   build_dir/fontspector/_summary.json    — the aggregate the breakdown panels read
//!
//! The binary is a pinned crates.io release, cargo-installed once into <data_dir>/tools/ (override
//! with --fontspector-bin). We run with --skip-network so the pass is deterministic and fast.

use crate::config::Config;
use crate::model::{FontspectorView, FsCheck, FsCounts, FsFamily};
use crate::persist;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Entry point for `--fontspector`. Returns a process exit code.
pub fn run_pass(cfg: &Config) -> i32 {
    let (bin, version) = match ensure_binary(cfg) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fontspector: {}", e);
            return 1;
        }
    };
    eprintln!("{} · profile={} · build-dir={}", version, cfg.fontspector_profile, cfg.build_dir.display());

    let out_root = cfg.build_dir.join("out");
    let families = enumerate_built(&out_root);
    if families.is_empty() {
        eprintln!("no built fonts found under {} — build some families first.", out_root.display());
        return 1;
    }
    let fsdir = persist::fontspector_dir(&cfg.build_dir);
    let _ = std::fs::create_dir_all(&fsdir);

    let total = families.len();
    let mut ran = 0usize;
    for (i, (slug, fonts)) in families.iter().enumerate() {
        let resfile = fsdir.join(format!("{}.json", slug.replace('/', "__")));
        if resfile.is_file() && !cfg.fontspector_rerun {
            continue; // already QA'd (use --fontspector-rerun to redo)
        }
        match run_one(&bin, cfg, slug, fonts, &version) {
            Ok(fam) => {
                let _ = std::fs::write(&resfile, serde_json::to_string(&fam.json).unwrap_or_default());
                let c = &fam.summary.counts;
                eprintln!("[{}/{}] {:<40} {} fail · {} warn · {} pass{}",
                    i + 1, total, slug, c.fail + c.fatal + c.error, c.warn, c.pass,
                    if c.error > 0 { format!(" · {} ERROR", c.error) } else { String::new() });
                ran += 1;
            }
            Err(e) => eprintln!("[{}/{}] {:<40} fontspector failed: {}", i + 1, total, slug, e),
        }
    }

    // ---- aggregate every stored per-family result into _summary.json ----
    let view = aggregate(&fsdir, &cfg.fontspector_profile, &version);
    let _ = std::fs::write(fsdir.join("_summary.json"), serde_json::to_string(&view).unwrap_or_default());
    eprintln!("done: {} QA'd this pass · {} total · {} families with FAIL/FATAL/ERROR",
        ran, view.families_checked,
        view.per_family.iter().filter(|f| f.counts.fail + f.counts.fatal + f.counts.error > 0).count());
    eprintln!("view: gflib-build --attach   (or --ui web)   ·   results in {}", fsdir.display());
    0
}

/// Resolve the fontspector binary + its exact version string. Order: --fontspector-bin → a cached
/// install under <data_dir>/tools/fontspector-<ver>/ → cargo-install the pinned release there.
fn ensure_binary(cfg: &Config) -> Result<(PathBuf, String), String> {
    if let Some(b) = &cfg.fontspector_bin {
        let v = binary_version(b)?;
        return Ok((b.clone(), v));
    }
    let root = cfg.data_dir.join("tools").join(format!("fontspector-{}", cfg.fontspector_version));
    let bin = root.join("bin").join("fontspector");
    if bin.is_file() {
        let v = binary_version(&bin)?;
        return Ok((bin, v));
    }
    eprintln!("installing fontspector {} (one-time, via cargo install)…", cfg.fontspector_version);
    let status = Command::new("cargo")
        .args(["install", "fontspector", "--version", &cfg.fontspector_version, "--locked", "--root"])
        .arg(&root)
        .status()
        .map_err(|e| format!("could not run cargo install (is cargo on PATH?): {}", e))?;
    if !status.success() || !bin.is_file() {
        return Err(format!("cargo install fontspector@{} failed (install it manually and pass --fontspector-bin)", cfg.fontspector_version));
    }
    let v = binary_version(&bin)?;
    Ok((bin, v))
}

/// `fontspector --version` → the exact version string (e.g. "fontspector 1.6.0"), saved as metadata.
fn binary_version(bin: &Path) -> Result<String, String> {
    let out = Command::new(bin).arg("--version").output().map_err(|e| format!("running {} --version: {}", bin.display(), e))?;
    let s = String::from_utf8_lossy(&out.stdout);
    let v = s.lines().next().unwrap_or("").trim().to_string();
    if v.is_empty() { Err("fontspector --version produced no output".into()) } else { Ok(v) }
}

struct OneResult {
    json: Value,        // what we persist per family
    summary: FsFamily,  // the family's counts + worst (for the eprintln + a sanity check)
}

/// Run fontspector on one family's fonts and shape the result we persist.
fn run_one(bin: &Path, cfg: &Config, slug: &str, fonts: &[PathBuf], version: &str) -> Result<OneResult, String> {
    let tmp = persist::fontspector_dir(&cfg.build_dir).join(format!(".{}.tmp.json", slug.replace('/', "__")));
    let mut cmd = Command::new(bin);
    cmd.args(["--profile", &cfg.fontspector_profile, "--quiet", "--skip-network", "--json"]).arg(&tmp);
    for f in fonts {
        cmd.arg(f);
    }
    let status = cmd.status().map_err(|e| format!("spawn fontspector: {}", e))?;
    let _ = status; // fontspector exits non-zero when checks FAIL; that's expected, not an error
    let raw = std::fs::read_to_string(&tmp).map_err(|e| format!("no fontspector json ({}): {}", tmp.display(), e))?;
    let _ = std::fs::remove_file(&tmp);
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("parse fontspector json: {}", e))?;

    // collapse results.<file>.<section>.[checks] → per check-id the WORST status across all the
    // family's fonts, with the check title.
    let mut checks: BTreeMap<String, (String, String)> = BTreeMap::new(); // id -> (title, worst status)
    if let Some(files) = v.get("results").and_then(|r| r.as_object()) {
        for sections in files.values() {
            if let Some(secs) = sections.as_object() {
                for arr in secs.values() {
                    for c in arr.as_array().into_iter().flatten() {
                        let id = c.get("check_id").and_then(|x| x.as_str()).unwrap_or("");
                        if id.is_empty() {
                            continue;
                        }
                        let title = c.get("check_name").and_then(|x| x.as_str()).unwrap_or(id).to_string();
                        let st = c.get("worst_status").and_then(|x| x.as_str()).unwrap_or("PASS").to_string();
                        let e = checks.entry(id.to_string()).or_insert_with(|| (title.clone(), "PASS".into()));
                        if !e.0.is_empty() {
                            e.0 = title;
                        }
                        if severity(&st) > severity(&e.1) {
                            e.1 = st;
                        }
                    }
                }
            }
        }
    }
    let mut counts = FsCounts::default();
    let mut worst = "PASS".to_string();
    let check_list: Vec<Value> = checks.iter().map(|(id, (title, st))| {
        bump(&mut counts, st);
        if severity(st) > severity(&worst) {
            worst = st.clone();
        }
        serde_json::json!({"id": id, "title": title, "status": st})
    }).collect();

    let json = serde_json::json!({
        "slug": slug,
        "fontspector_version": version,
        "profile": cfg.fontspector_profile,
        "checks": check_list,
        "counts": counts,
    });
    Ok(OneResult { json, summary: FsFamily { slug: slug.to_string(), counts, worst } })
}

/// Severity ordering so we can pick the WORST status seen.
fn severity(s: &str) -> u8 {
    match s {
        "SKIP" => 0,
        "PASS" => 1,
        "INFO" => 2,
        "WARN" => 3,
        "FAIL" => 4,
        "FATAL" => 5,
        "ERROR" => 6,
        _ => 1,
    }
}

fn bump(c: &mut FsCounts, status: &str) {
    match status {
        "PASS" => c.pass += 1,
        "WARN" => c.warn += 1,
        "FAIL" => c.fail += 1,
        "FATAL" => c.fatal += 1,
        "ERROR" => c.error += 1,
        "SKIP" => c.skip += 1,
        "INFO" => c.info += 1,
        _ => {}
    }
}

/// All `out/<slug__>/` dirs that contain at least one built font → (slug, font paths).
fn enumerate_built(out_root: &Path) -> Vec<(String, Vec<PathBuf>)> {
    let mut v = Vec::new();
    let entries = match std::fs::read_dir(out_root) {
        Ok(e) => e,
        Err(_) => return v,
    };
    for e in entries.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        let dirname = e.file_name().to_string_lossy().to_string();
        let mut fonts = collect_fonts(&e.path());
        fonts.sort();
        if fonts.is_empty() {
            continue;
        }
        let slug = dirname.replacen("__", "/", 1); // out dir name = slug with '/'→'__'
        v.push((slug, fonts));
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// Recursively collect .ttf/.otf files under a family's output dir (covers out/<slug>/{fontc,fontmake}/…).
fn collect_fonts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(collect_fonts(&p));
            } else if matches!(p.extension().and_then(|x| x.to_str()), Some("ttf") | Some("otf")) {
                out.push(p);
            }
        }
    }
    out
}

/// Read every per-family result file and build the aggregate the panels read.
fn aggregate(fsdir: &Path, profile: &str, version: &str) -> FontspectorView {
    let mut per_family: Vec<FsFamily> = Vec::new();
    let mut per_check: BTreeMap<String, FsCheck> = BTreeMap::new();
    let mut total = FsCounts::default();

    let entries = std::fs::read_dir(fsdir).into_iter().flatten().flatten();
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") || name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        let v: Value = match std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            Some(v) => v,
            None => continue,
        };
        let slug = v.get("slug").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let counts: FsCounts = serde_json::from_value(v.get("counts").cloned().unwrap_or_default()).unwrap_or_default();
        let mut worst = "PASS".to_string();
        for c in v.get("checks").and_then(|c| c.as_array()).into_iter().flatten() {
            let id = c.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let title = c.get("title").and_then(|x| x.as_str()).unwrap_or(&id).to_string();
            let st = c.get("status").and_then(|x| x.as_str()).unwrap_or("PASS").to_string();
            if severity(&st) > severity(&worst) {
                worst = st.clone();
            }
            let ck = per_check.entry(id.clone()).or_insert_with(|| FsCheck { id: id.clone(), title: title.clone(), ..Default::default() });
            if ck.title.is_empty() {
                ck.title = title;
            }
            bump(&mut ck.counts, &st);
            match st.as_str() {
                "FAIL" | "FATAL" | "ERROR" => ck.fail_families.push(slug.clone()),
                "WARN" => ck.warn_families.push(slug.clone()),
                _ => {}
            }
        }
        accumulate(&mut total, &counts);
        per_family.push(FsFamily { slug, counts, worst });
    }
    per_family.sort_by(|a, b| severity(&b.worst).cmp(&severity(&a.worst)).then(a.slug.cmp(&b.slug)));
    let mut checks: Vec<FsCheck> = per_check.into_values().collect();
    // most-actionable checks first: by FAIL+FATAL+ERROR, then WARN
    checks.sort_by(|a, b| {
        let af = a.counts.fail + a.counts.fatal + a.counts.error;
        let bf = b.counts.fail + b.counts.fatal + b.counts.error;
        bf.cmp(&af).then(b.counts.warn.cmp(&a.counts.warn)).then(a.id.cmp(&b.id))
    });
    FontspectorView {
        version: version.to_string(),
        profile: profile.to_string(),
        ts: crate::util::now(),
        families_checked: per_family.len(),
        total,
        per_check: checks,
        per_family,
    }
}

fn accumulate(a: &mut FsCounts, b: &FsCounts) {
    a.pass += b.pass;
    a.warn += b.warn;
    a.fail += b.fail;
    a.fatal += b.fatal;
    a.error += b.error;
    a.skip += b.skip;
    a.info += b.info;
}
