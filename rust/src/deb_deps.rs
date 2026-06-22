//! `--package-deb-deps`: gflib-build drives the archive-pure dependency-packaging burn-down itself.
//!
//! It computes, from gftools-builder3's `Cargo.lock`, the crates NOT already in Debian (the git-pinned
//! font crates + a curated set of crates.io crates Debian lacks) plus the two tool binaries, in
//! topological (leaves-first) order — then runs `debcargo` → `dpkg-buildpackage`/`sbuild` → publish to a
//! local apt repo, idempotently. Pure-Rust planning replaces the old Python `gen_manifest.py`.
//!
//! EXECUTION is host-only (debcargo/sbuild are not in the build VM): when the tools are absent the run is
//! a dry-run that prints the exact commands. The planning and command construction are unit-tested; the
//! per-package recipes are deliberately small + commented for host tuning.

use crate::config::Config;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// crates.io crates NOT in Debian (a "registry" source does NOT imply Debian has it — the fontations
/// family is crates.io-published yet absent). `verify-debian.sh` finds others on the host.
const SPECIALIST_MISSING: &[&str] = &[
    "openstep-plist", "norad", "serde_yaml_ng", "ttf2woff2", "ascii-dag",
    "google-fonts-languages", "google-fonts-subsets", "glyphslib", "yeslogic-unicode-blocks",
    "font-types", "read-fonts", "write-fonts", "skrifa",
];
/// the two binary tool packages (built with dh-cargo on top of the library crates)
const TOOLS: &[&str] = &["fontc", "gftools-builder"];
/// fontspector/QA crates: feature-gated OFF so they never enter the build-deps graph
const FONTSPECTOR: &[&str] = &[
    "fontspector-checkapi", "fontspector-checkhelper", "fontspector-hotfix",
    "fontspector-profile-fontwerk", "fontspector-profile-googlefonts", "fontspector-profile-iso15008",
    "fontspector-profile-opentype", "fontspector-profile-universal", "sr-aef", "shaperglot",
];

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub enum Src { Git, CratesIo }
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub enum Kind { Crate, Tool }

#[derive(Debug, Clone, serde::Serialize)]
pub struct DepPkg {
    pub krate: String,
    pub version: String,
    pub src: Src,
    pub kind: Kind,
    pub deps_in_set: Vec<String>, // its deps that are ALSO in the to-package set (must build first)
}

#[derive(serde::Deserialize)]
struct Lock { #[serde(default)] package: Vec<LockPkg> }
#[derive(serde::Deserialize)]
struct LockPkg {
    name: String,
    #[serde(default)] version: String,
    #[serde(default)] source: String,
    #[serde(default)] dependencies: Vec<String>,
}

/// A Cargo.lock dependency entry is "name", "name ver", or "name ver (source)" — take the name.
fn dep_name(d: &str) -> &str { d.split_whitespace().next().unwrap_or(d) }

/// Compute the archive-pure burn-down (leaves-first) from a Cargo.lock.
pub fn plan(lock_path: &Path) -> Result<Vec<DepPkg>, String> {
    let txt = std::fs::read_to_string(lock_path).map_err(|e| format!("read {}: {e}", lock_path.display()))?;
    plan_from_str(&txt)
}

/// Plan from Cargo.lock text (split out for unit testing).
pub fn plan_from_str(txt: &str) -> Result<Vec<DepPkg>, String> {
    let lock: Lock = toml::from_str(txt).map_err(|e| format!("parse Cargo.lock: {e}"))?;
    let by_name: BTreeMap<&str, &LockPkg> = lock.package.iter().map(|p| (p.name.as_str(), p)).collect();
    let is_git = |p: &LockPkg| p.source.starts_with("git+");
    let fontspector: BTreeSet<&str> = FONTSPECTOR.iter().copied().collect();
    let specialist: BTreeSet<&str> = SPECIALIST_MISSING.iter().copied().collect();
    let tools: BTreeSet<&str> = TOOLS.iter().copied().collect();

    // the set that needs from-scratch packaging: git-sourced + specialist-missing crates.io
    let mut to_pkg: BTreeSet<String> = BTreeSet::new();
    for p in &lock.package {
        if fontspector.contains(p.name.as_str()) { continue; }
        // git crates + specialist-missing crates.io crates + the tool binaries (gftools-builder is the
        // root workspace crate with no `source`, so it must be named explicitly).
        if is_git(p) || specialist.contains(p.name.as_str()) || tools.contains(p.name.as_str()) {
            to_pkg.insert(p.name.clone());
        }
    }

    // iterative DFS post-order = topological (a crate emitted after its in-set deps)
    let mut order: Vec<String> = Vec::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    for root in &to_pkg {
        // stack of (node, children_pushed?)
        let mut stack: Vec<(String, bool)> = vec![(root.clone(), false)];
        let mut on_stack: BTreeSet<String> = BTreeSet::new();
        while let Some((n, expanded)) = stack.pop() {
            if done.contains(&n) { continue; }
            if expanded {
                done.insert(n.clone());
                order.push(n);
                continue;
            }
            on_stack.insert(n.clone());
            stack.push((n.clone(), true));
            if let Some(p) = by_name.get(n.as_str()) {
                for d in &p.dependencies {
                    let dn = dep_name(d);
                    if to_pkg.contains(dn) && !done.contains(dn) && !on_stack.contains(dn) {
                        stack.push((dn.to_string(), false));
                    }
                }
            }
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for n in &order {
        let Some(p) = by_name.get(n.as_str()) else { continue };
        let deps_in_set: Vec<String> = p.dependencies.iter()
            .map(|d| dep_name(d).to_string())
            .filter(|d| to_pkg.contains(d) && d != n)
            .collect::<BTreeSet<_>>().into_iter().collect();
        out.push(DepPkg {
            krate: n.clone(),
            version: p.version.clone(),
            src: if is_git(p) { Src::Git } else { Src::CratesIo },
            kind: if tools.contains(n.as_str()) { Kind::Tool } else { Kind::Crate },
            deps_in_set,
        });
    }
    Ok(out)
}

/// The shell commands to package ONE entry, given the work dir, the apt-repo dir, the vendor dir (for
/// git crates, whose source isn't on crates.io), and the build front-end ("sbuild" or "dpkg-buildpackage").
/// Returned as strings so they can be unit-tested and printed in dry-run mode. Host-tunable.
pub fn package_commands(pkg: &DepPkg, work: &Path, apt: &Path, vendor: &Path, builder: &str) -> Vec<String> {
    let w = work.display();
    let a = apt.display();
    let v = vendor.display();
    let build = if builder == "sbuild" {
        // sbuild resolves Build-Depends from the local apt repo we accumulate
        format!("sbuild --no-clean-source --extra-repository='deb [trusted=yes] file://{a} ./'")
    } else {
        "dpkg-buildpackage -b -uc -us".to_string()
    };
    let mut c = Vec::new();
    match (pkg.kind, pkg.src) {
        (Kind::Crate, Src::CratesIo) => {
            // debcargo can pull the published crate straight from the registry
            c.push(format!("cd {w} && debcargo package {} {}", pkg.krate, pkg.version));
            c.push(format!("cd {w}/{}-{} && {build}", pkg.krate, pkg.version));
        }
        (Kind::Crate, Src::Git) => {
            // git-pinned crate: package from the vendored source tree (cargo vendor), version-encoding
            // the pin; debcargo's exact local-source flag varies by version — confirm on host.
            c.push(format!("debcargo package --directory {w}/{} {} {} --crate-path {v}/{}",
                pkg.krate, pkg.krate, pkg.version, pkg.krate));
            c.push(format!("cd {w}/{} && {build}", pkg.krate));
        }
        (Kind::Tool, _) => {
            // tool binary via dh-cargo (ships /usr/bin/<tool>); Build-Depends = the librust-*-dev above
            c.push(format!("cd {w}/{} && dh $@ --buildsystem=cargo   # (debian/ from dh-cargo)", pkg.krate));
            c.push(format!("cd {w}/{} && {build}", pkg.krate));
        }
    }
    // publish whatever .debs the build produced into the local apt repo
    c.push(format!("cp {w}/*.deb {a}/ 2>/dev/null || true"));
    c
}

/// Has this package already been built into the local apt repo? (idempotency)
fn already_built(apt: &Path, pkg: &DepPkg) -> bool {
    let needle = match pkg.kind {
        Kind::Tool => pkg.krate.replace('_', "-"),
        Kind::Crate => format!("librust-{}-dev", pkg.krate.replace('_', "-")),
    };
    std::fs::read_dir(apt).map(|rd| rd.flatten().any(|e| {
        e.file_name().to_string_lossy().starts_with(&needle)
            && e.file_name().to_string_lossy().ends_with(".deb")
    })).unwrap_or(false)
}

/// Locate gftools-builder3's Cargo.lock (the discovered toolchain checkout, sibling of the data dir).
fn locate_builder3_lock(cfg: &Config) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(abs) = std::path::absolute(&cfg.build_dir) {
        let mut cur = abs.parent().map(|p| p.to_path_buf());
        for _ in 0..5 { if let Some(c) = cur { roots.push(c.clone()); cur = c.parent().map(|p| p.to_path_buf()); } }
    }
    for r in roots {
        let p = r.join("gftools-builder3").join("Cargo.lock");
        if p.is_file() { return Some(p); }
    }
    None
}

/// `--package-deb-deps`: compute the plan, write it out, and drive the burn-down (or dry-run when the
/// host tooling is absent). Idempotent + continue-on-failure; per-package results in deb-deps/results.json.
pub fn run_package_deb_deps(cfg: &Config) {
    let Some(lock) = locate_builder3_lock(cfg) else {
        eprintln!("package-deb-deps: could not locate gftools-builder3/Cargo.lock next to the data dir");
        return;
    };
    let plan = match plan(&lock) {
        Ok(p) => p,
        Err(e) => { eprintln!("package-deb-deps: {e}"); return; }
    };
    let work = cfg.build_dir.join("deb-deps");
    let apt = work.join("apt");
    let vendor = work.join("vendor");
    let _ = std::fs::create_dir_all(&apt);

    // gflib-build now generates the manifest itself (no Python gen_manifest.py)
    if let Ok(txt) = serde_json::to_string_pretty(&serde_json::json!({ "count": plan.len(), "packages": plan })) {
        let _ = std::fs::write(work.join("manifest.json"), txt);
    }

    let have = |t: &str| crate::deb::on_path(t);
    let (has_debcargo, has_cargo) = (have("debcargo"), have("cargo"));
    let builder = if have("sbuild") { "sbuild" } else { "dpkg-buildpackage" };
    let can_run = has_debcargo && (have("sbuild") || have("dpkg-buildpackage"));
    let git_count = plan.iter().filter(|p| p.src == Src::Git).count();

    eprintln!("package-deb-deps: {} packages to build ({} git, {} crates.io, {} tools); tools: debcargo={} cargo={} build={}",
        plan.len(), git_count,
        plan.iter().filter(|p| p.src == Src::CratesIo && p.kind == Kind::Crate).count(),
        plan.iter().filter(|p| p.kind == Kind::Tool).count(),
        has_debcargo, has_cargo, builder);
    if !can_run {
        eprintln!("package-deb-deps: DRY RUN (missing debcargo and/or a build front-end). Commands per package follow.");
    }
    if git_count > 0 && can_run {
        // git crates aren't on crates.io: vendor the whole graph once so debcargo has their source
        eprintln!("package-deb-deps: vendoring git crates → {}", vendor.display());
        let _ = run_logged(&format!("cd {} && cargo vendor {}", lock.parent().unwrap().display(), vendor.display()), &work);
    }

    let mut results = Vec::new();
    let (mut built, mut skipped, mut failed) = (0, 0, 0);
    for pkg in &plan {
        let cmds = package_commands(pkg, &work, &apt, &vendor, builder);
        if already_built(&apt, pkg) {
            skipped += 1;
            results.push(serde_json::json!({"crate": pkg.krate, "status": "skipped (already built)"}));
            continue;
        }
        if !can_run {
            results.push(serde_json::json!({"crate": pkg.krate, "status": "dry-run", "commands": cmds}));
            eprintln!("# {} {}", pkg.krate, pkg.version);
            for c in &cmds { eprintln!("    {c}"); }
            continue;
        }
        let _ = std::fs::create_dir_all(work.join(&pkg.krate));
        let mut ok = true;
        let mut last = String::new();
        for c in &cmds {
            match run_logged(c, &work) {
                Ok(_) => {}
                Err(e) => { ok = false; last = e; break; }
            }
        }
        if ok && already_built(&apt, pkg) {
            built += 1;
            results.push(serde_json::json!({"crate": pkg.krate, "status": "built"}));
        } else {
            failed += 1;
            results.push(serde_json::json!({"crate": pkg.krate, "status": "failed", "error": last}));
            eprintln!("package-deb-deps: FAILED {} — {}", pkg.krate, last);
        }
    }
    if can_run {
        // regenerate the apt index so the next sbuild can resolve what we just published
        let _ = run_logged(&format!("cd {} && dpkg-scanpackages -m . /dev/null > Packages 2>/dev/null && gzip -9kf Packages", apt.display()), &work);
    }
    let doc = serde_json::json!({ "built": built, "skipped": skipped, "failed": failed,
        "dry_run": !can_run, "results": results });
    if let Ok(txt) = serde_json::to_string_pretty(&doc) {
        let _ = std::fs::write(work.join("results.json"), txt);
    }
    eprintln!("package-deb-deps: built {built}, skipped {skipped}, failed {failed} → {}", apt.display());
}

/// Run one shell command, appending stdout+stderr to deb-deps/build.log. Ok(()) on success.
fn run_logged(cmd: &str, work: &Path) -> Result<(), String> {
    let out = std::process::Command::new("sh").arg("-c").arg(cmd).current_dir(work).output()
        .map_err(|e| format!("spawn: {e}"))?;
    let log = work.join("build.log");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        let _ = writeln!(f, "$ {cmd}");
        let _ = f.write_all(&out.stdout);
        let _ = f.write_all(&out.stderr);
    }
    if out.status.success() { Ok(()) }
    else { Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("command failed").to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LOCK: &str = r#"
version = 4

[[package]]
name = "font-types"
version = "0.12.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "read-fonts"
version = "0.40.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = ["font-types"]

[[package]]
name = "fontdrasil"
version = "0.4.0"
source = "git+https://github.com/googlefonts/fontc?rev=abc#abc123"
dependencies = ["read-fonts", "serde"]

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "fontc"
version = "0.6.0"
source = "git+https://github.com/googlefonts/fontc?rev=abc#abc123"
dependencies = ["fontdrasil", "read-fonts"]

[[package]]
name = "shaperglot"
version = "0.1.0"
source = "git+https://example/shaperglot#deadbeef"
"#;

    #[test]
    fn plan_orders_leaves_first_and_classifies() {
        let p = plan_from_str(LOCK).unwrap();
        let names: Vec<&str> = p.iter().map(|x| x.krate.as_str()).collect();
        // serde is in Debian → excluded; shaperglot is fontspector → excluded
        assert!(!names.contains(&"serde"), "in-Debian crate excluded: {names:?}");
        assert!(!names.contains(&"shaperglot"), "fontspector crate excluded: {names:?}");
        // topo: each dep-in-set appears before the crate that needs it
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("font-types") < pos("read-fonts"));
        assert!(pos("read-fonts") < pos("fontdrasil"));
        assert!(pos("fontdrasil") < pos("fontc"));
        // classification
        let fontc = p.iter().find(|x| x.krate == "fontc").unwrap();
        assert_eq!(fontc.kind, Kind::Tool);
        assert_eq!(fontc.src, Src::Git);
        let ft = p.iter().find(|x| x.krate == "font-types").unwrap();
        assert_eq!(ft.kind, Kind::Crate);
        assert_eq!(ft.src, Src::CratesIo);
        // deps_in_set excludes the in-Debian serde
        assert_eq!(p.iter().find(|x| x.krate == "fontdrasil").unwrap().deps_in_set, vec!["read-fonts"]);
    }

    #[test]
    fn commands_match_kind_and_source() {
        let cratesio = DepPkg { krate: "norad".into(), version: "0.1.0".into(), src: Src::CratesIo, kind: Kind::Crate, deps_in_set: vec![] };
        let cmds = package_commands(&cratesio, Path::new("/w"), Path::new("/w/apt"), Path::new("/w/vendor"), "dpkg-buildpackage");
        assert!(cmds.iter().any(|c| c.contains("debcargo package norad 0.1.0")), "{cmds:?}");
        let tool = DepPkg { krate: "fontc".into(), version: "0.6.0".into(), src: Src::Git, kind: Kind::Tool, deps_in_set: vec![] };
        let tcmds = package_commands(&tool, Path::new("/w"), Path::new("/w/apt"), Path::new("/w/vendor"), "dpkg-buildpackage");
        assert!(tcmds.iter().any(|c| c.contains("dh $@ --buildsystem=cargo")), "{tcmds:?}");
    }
}
