//! Cohort virtual-environment manager (R2) — a faithful port of the Python `VenvManager`. Families
//! with identical `requirements.txt` share one venv keyed by a content hash; the `base` cohort holds
//! everything with no/standard requirements. Each venv carries a `.gflib-installed` marker (a hash of
//! the requirements it was built for) so a stale/half-installed venv is rebuilt, never reused — and a
//! self-healing install drops pins pip can't satisfy / a base pin a cohort conflicts with, then
//! retries. PIN_OVERRIDES (compreffor>=0.5.6, drop the fontbakery extra) are forced up front.
//!
//! Hashes use `sha1sum`/`sha256sum` (coreutils) so the cohort key and readiness marker are
//! BYTE-IDENTICAL to the Python tool's — meaning the Rust port REUSES the existing venvs/ directory
//! created by the Python daemon (a real drop-in, not a parallel set).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

pub const REQ_FILES: [&str; 2] = ["requirements.txt", "requirements.in"];

/// Forced version overrides applied up front to every cohort (mirror of the Python PIN_OVERRIDES).
pub fn pin_override(pkg: &str) -> Option<&'static str> {
    match pkg {
        // compreffor <0.5.6 has no cp313 wheel; its sdist imports pkg_resources at build time → dies.
        "compreffor" => Some("compreffor>=0.5.6"),
        // fontbakery[googlefonts] pulls a nonexistent extra → endless pip backtracking; QA-only.
        "fontbakery" => Some("fontbakery"),
        _ => None,
    }
}

// ---------- pure requirement helpers (unit-tested) ----------

/// Package name from a requirements line, or "" for blank/comment/option/URL lines.
pub fn req_pkg_name(line: &str) -> String {
    let s = line.trim();
    if s.is_empty() || s.starts_with('#') || s.starts_with('-') || s.contains("://") {
        return String::new();
    }
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        let ok = if i == 0 {
            c.is_ascii_alphanumeric() || c == '_' || c == '.'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
        };
        if ok {
            out.push(c);
        } else {
            break;
        }
    }
    out.to_lowercase()
}

pub fn normalize_requirements(text: &str) -> String {
    let mut lines: Vec<String> = text
        .lines()
        .map(|ln| ln.split('#').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    lines.sort();
    lines.join("\n")
}

pub fn cohort_key_for(req_text: &str) -> String {
    let norm = normalize_requirements(req_text);
    if norm.is_empty() {
        return "base".to_string();
    }
    let h = sha_hex("sha1sum", &norm);
    format!("c-{}", &h[..h.len().min(12)])
}

/// Drop the version pin (keep just the package name) for any requirement whose package is in `relax`.
pub fn relax_requirements(lines: &[String], relax: &HashSet<String>) -> Vec<String> {
    lines
        .iter()
        .map(|ln| {
            let pkg = req_pkg_name(ln);
            if !pkg.is_empty() && relax.contains(&pkg) {
                format!("{}    # auto-relaxed by gflib-build: pinned version unavailable on PyPI", pkg)
            } else {
                ln.clone()
            }
        })
        .collect()
}

/// Rewrite any requirement whose package has a PIN_OVERRIDE. Returns (lines, applied_packages).
pub fn apply_pin_overrides(lines: &[String]) -> (Vec<String>, Vec<String>) {
    let mut out = Vec::with_capacity(lines.len());
    let mut applied = Vec::new();
    for ln in lines {
        let pkg = req_pkg_name(ln);
        if let Some(spec) = pin_override(&pkg) {
            out.push(format!("{}    # gflib-build: forced ({}'s pinned version cannot build here)", spec, pkg));
            applied.push(pkg);
        } else {
            out.push(ln.clone());
        }
    }
    (out, applied)
}

/// Packages pip reported it could not satisfy (a pinned version absent from the index).
pub fn parse_unsatisfiable(text: &str) -> HashSet<String> {
    let mut bad = HashSet::new();
    for marker in [
        "Could not find a version that satisfies the requirement ",
        "No matching distribution found for ",
    ] {
        let mut rest = text;
        while let Some(pos) = rest.find(marker) {
            let after = &rest[pos + marker.len()..];
            let tok = take_pkg(after);
            if !tok.is_empty() {
                bad.insert(tok.to_lowercase());
            }
            rest = &after[tok.len().max(1).min(after.len())..];
        }
    }
    bad
}

/// Base pins a cohort's own dep conflicts with (ResolutionImpossible) — only ones WE control.
pub fn parse_conflict_pins(text: &str, base_pkgs: &HashSet<String>) -> HashSet<String> {
    let low = text.to_lowercase();
    if !low.contains("resolutionimpossible") && !low.contains("conflicting dependencies") {
        return HashSet::new();
    }
    let mut out = HashSet::new();
    let marker = "ser requested "; // matches "The user requested " / "the user requested "
    let mut rest = text;
    while let Some(pos) = rest.find(marker) {
        let after = &rest[pos + marker.len()..];
        let tok = take_pkg(after);
        if !tok.is_empty() && after[tok.len().min(after.len())..].trim_start().starts_with("==") {
            out.insert(tok.to_lowercase());
        }
        rest = &after[tok.len().max(1).min(after.len())..];
    }
    out.intersection(base_pkgs).cloned().collect()
}

/// A missing SYSTEM library (not a pin we can fix) → a short "<lib> (install <pkg>)" hint, else None.
pub fn scan_missing_system_dep(text: &str) -> Option<String> {
    if let Some(lib) = between(text, "Dependency \"", "\" not found") {
        return Some(syslib_hint(&lib));
    }
    if let Some(lib) = between(text, "No package '", "' found") {
        return Some(syslib_hint(&lib));
    }
    if let Some(pos) = text.find("fatal error:") {
        let after = &text[pos + "fatal error:".len()..];
        if let Some(hp) = after.find(".h:") {
            if after[hp..].contains("No such file") {
                let hdr = after[..hp + 2].trim();
                return Some(format!("C headers <{}> (install the matching -dev package)", hdr));
            }
        }
    }
    None
}

fn syslib_hint(lib: &str) -> String {
    let pkg = match lib.to_lowercase().as_str() {
        "cairo" => Some("libcairo2-dev"),
        "freetype2" | "freetype" => Some("libfreetype-dev"),
        "fontconfig" => Some("libfontconfig1-dev"),
        "harfbuzz" => Some("libharfbuzz-dev"),
        "glib-2.0" => Some("libglib2.0-dev"),
        "libffi" => Some("libffi-dev"),
        _ => None,
    };
    match pkg {
        Some(p) => format!("{} (install {})", lib, p),
        None => format!("{} (install its -dev package)", lib),
    }
}

fn take_pkg(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.' || *c == '-')
        .collect()
}

fn between(text: &str, start: &str, end: &str) -> Option<String> {
    let i = text.find(start)? + start.len();
    let j = text[i..].find(end)? + i;
    Some(text[i..j].to_string())
}

/// Read a repo's requirements at a commit WITHOUT extracting — read-only `git show` on the mirror.
pub fn read_requirements_from_mirror(mirror: &Path, commit: &str) -> String {
    for r in REQ_FILES {
        let out = Command::new("git")
            .args(["--git-dir", &mirror.to_string_lossy(), "show", &format!("{}:{}", commit, r)])
            .output();
        if let Ok(o) = out {
            if o.status.success() {
                return String::from_utf8_lossy(&o.stdout).to_string();
            }
        }
    }
    String::new()
}

/// Read the family's requirements from its extracted work tree (post-checkout). Parity API; the build
/// path reads from the mirror (read-only) instead, but `--cohorts-report` / pre-build will use this.
#[allow(dead_code)]
pub fn read_requirements(work: &Path) -> String {
    for r in REQ_FILES {
        let p = work.join(r);
        if p.is_file() {
            if let Ok(t) = std::fs::read_to_string(&p) {
                return t;
            }
        }
    }
    String::new()
}

/// sha1sum/sha256sum of a string (coreutils) — matches Python's hexdigest so venvs are interchangeable.
fn sha_hex(tool: &str, input: &str) -> String {
    let child = Command::new(tool).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(input.as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

// ---------- the manager ----------

struct Inner {
    locks: HashMap<String, Arc<Mutex<()>>>,
    ready: HashMap<String, String>, // cohort key -> python path
    relaxed: HashSet<String>,       // base pins auto-relaxed once, shared by cohorts
    override_recorded: HashSet<String>,
    pub relaxations: Vec<String>,
}

pub struct VenvManager {
    root: PathBuf,
    pip_cache: PathBuf,
    base_python: String,
    base_req: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl VenvManager {
    pub fn new(build_dir: &Path, base_python: &str, base_requirements: Option<PathBuf>) -> Self {
        let root = build_dir.join("venvs");
        let pip_cache = build_dir.join("pip-cache");
        let _ = std::fs::create_dir_all(&root);
        let _ = std::fs::create_dir_all(&pip_cache);
        VenvManager {
            root,
            pip_cache,
            base_python: base_python.to_string(),
            base_req: base_requirements,
            inner: Mutex::new(Inner {
                locks: HashMap::new(),
                ready: HashMap::new(),
                relaxed: HashSet::new(),
                override_recorded: HashSet::new(),
                relaxations: Vec::new(),
            }),
        }
    }

    pub fn relaxations(&self) -> Vec<String> {
        self.inner.lock().unwrap().relaxations.clone()
    }

    pub fn ready_count(&self) -> usize {
        self.inner.lock().unwrap().ready.len()
    }

    pub fn ensure_base(&self) -> Result<String, String> {
        let (py, err) = self.create("base", "");
        if err.is_empty() {
            self.inner.lock().unwrap().ready.insert("base".into(), py.clone());
            Ok(py)
        } else {
            Err(format!("base venv creation failed: {}", err))
        }
    }

    fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        let mut inner = self.inner.lock().unwrap();
        inner.locks.entry(key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    /// Get (create-or-reuse) the venv python for a cohort. Returns (python_path, cohort_key, error).
    pub fn get_python<F: FnOnce(&str)>(&self, req_text: &str, on_install: F) -> (String, String, String) {
        let key = cohort_key_for(req_text);
        {
            let inner = self.inner.lock().unwrap();
            if let Some(py) = inner.ready.get(&key) {
                return (py.clone(), key, String::new());
            }
        }
        let lock = self.lock_for(&key);
        let _g = lock.lock().unwrap(); // serialize creation of THIS cohort under full parallelism
        {
            let inner = self.inner.lock().unwrap();
            if let Some(py) = inner.ready.get(&key) {
                return (py.clone(), key, String::new());
            }
        }
        on_install(&key);
        let (py, err) = self.create(&key, req_text);
        if err.is_empty() {
            self.inner.lock().unwrap().ready.insert(key.clone(), py.clone());
        }
        (py, key, err)
    }

    /// Create (or reuse) the venv for `key`. Faithful port of the Python `_create`.
    fn create(&self, key: &str, req_text: &str) -> (String, String) {
        let vdir = self.root.join(key);
        let py = vdir.join("bin").join("python");
        let ready = vdir.join(".gflib-installed");
        let log = self.root.join(format!("{}.install.log", key));

        if let Some(br) = &self.base_req {
            if !br.is_file() {
                return (String::new(), format!(
                    "base requirements file not found: {} (stale base_requirements path — fix it or use --no-manage-venvs)",
                    br.display()
                ));
            }
        }
        let base_lines: Vec<String> = self
            .base_req
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| t.lines().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let cohort_lines: Vec<String> =
            if key == "base" { Vec::new() } else { req_text.lines().map(|s| s.to_string()).collect() };
        let requested: Vec<String> = base_lines.iter().chain(cohort_lines.iter()).cloned().collect();
        if !requested.iter().any(|l| !req_pkg_name(l).is_empty()) {
            return (String::new(),
                "no build requirements — the toolchain (gftools/fontmake/…) would be missing; manage-venvs needs a base requirements file".into());
        }

        // readiness hash incl. active pin overrides (so an overridden cohort rebuilds; others reuse)
        let mut ov: Vec<String> = requested
            .iter()
            .map(|l| req_pkg_name(l))
            .filter(|p| pin_override(p).is_some())
            .collect();
        ov.sort();
        ov.dedup();
        let mut key_text = requested.join("\n");
        if !ov.is_empty() {
            key_text.push_str("|ov:");
            key_text.push_str(&ov.iter().map(|p| format!("{}={}", p, pin_override(p).unwrap())).collect::<Vec<_>>().join(","));
        }
        let want_hash = {
            let h = sha_hex("sha256sum", &key_text);
            h[..h.len().min(16)].to_string()
        };

        // reuse only a venv whose marker matches THESE requirements
        if ready.exists() && py.exists() {
            if let Ok(m) = std::fs::read_to_string(&ready) {
                if m.trim() == want_hash {
                    return (py.to_string_lossy().to_string(), String::new());
                }
            }
        }
        let _ = std::fs::remove_dir_all(&vdir);
        let rc = Command::new(&self.base_python).args(["-m", "venv", &vdir.to_string_lossy()]).output();
        match rc {
            Ok(o) if o.status.success() => {}
            Ok(o) => return (String::new(), format!("venv create rc={:?}: {}", o.status.code(),
                String::from_utf8_lossy(&o.stdout).chars().take(200).collect::<String>())),
            Err(e) => return (String::new(), format!("venv create failed: {}", e)),
        }
        // seed setuptools+wheel (legacy sdists import pkg_resources at build time)
        let _ = Command::new(&py)
            .args(["-m", "pip", "install", "-q", "--disable-pip-version-check", "--cache-dir",
                   &self.pip_cache.to_string_lossy(), "setuptools", "wheel"])
            .output();

        let base_pkgs: HashSet<String> =
            base_lines.iter().map(|l| req_pkg_name(l)).filter(|p| !p.is_empty()).collect();
        let eff_path = vdir.join("effective-requirements.txt");

        // forced pin overrides up front (compreffor/fontbakery); record each once
        let (src_lines, overridden) = apply_pin_overrides(&requested);
        {
            let mut inner = self.inner.lock().unwrap();
            let mut uniq: Vec<String> = overridden.clone();
            uniq.sort();
            uniq.dedup();
            for p in uniq {
                if inner.override_recorded.insert(p.clone()) {
                    let spec = pin_override(&p).unwrap_or("");
                    inner.relaxations.push(format!("forced pin override: {} → {} (upstream pin can't build here)", p, spec));
                }
            }
        }

        let mut relax: HashSet<String> = self.inner.lock().unwrap().relaxed.iter().cloned().collect();
        let mut conflict_relax: HashSet<String> = HashSet::new();
        // SELF-HEALING install: drop a pin pip can't satisfy / a base pin a cohort conflicts with, retry.
        for attempt in 0..8 {
            let eff = relax_requirements(&src_lines, &relax);
            let _ = std::fs::write(&eff_path, eff.join("\n") + "\n");
            let mut header = String::new();
            if !relax.is_empty() {
                let mut r: Vec<_> = relax.iter().cloned().collect();
                r.sort();
                header = format!("# gflib-build attempt {}: auto-relaxed pins {:?}\n", attempt + 1, r);
            }
            // append the attempt header + run pip with stdout/stderr -> the cohort install log
            {
                let mut f = std::fs::OpenOptions::new().create(true)
                    .append(attempt != 0).write(true).truncate(attempt == 0).open(&log);
                if let Ok(ref mut lf) = f {
                    let _ = lf.write_all(header.as_bytes());
                }
            }
            let logf = std::fs::OpenOptions::new().create(true).append(true).open(&log);
            let status = match logf {
                Ok(lf) => {
                    let lf2 = lf.try_clone().ok();
                    Command::new(&py)
                        .args(["-m", "pip", "install", "--disable-pip-version-check", "--cache-dir",
                               &self.pip_cache.to_string_lossy(), "-r", &eff_path.to_string_lossy()])
                        .stdout(Stdio::from(lf))
                        .stderr(lf2.map(Stdio::from).unwrap_or(Stdio::null()))
                        .status()
                }
                Err(e) => return (String::new(), format!("open install log: {}", e)),
            };
            if matches!(&status, Ok(s) if s.success()) {
                let _ = std::fs::write(&ready, format!("{}\n", want_hash));
                // promote globally-bad base pins to the shared relaxed set (record once)
                let base_fixed: HashSet<String> = relax.difference(&conflict_relax).cloned().collect::<HashSet<_>>()
                    .intersection(&base_pkgs).cloned().collect();
                if !base_fixed.is_empty() {
                    let mut inner = self.inner.lock().unwrap();
                    let new: Vec<String> = base_fixed.difference(&inner.relaxed).cloned().collect();
                    for p in &base_fixed { inner.relaxed.insert(p.clone()); }
                    if !new.is_empty() {
                        let mut n = new.clone();
                        n.sort();
                        inner.relaxations.push(format!("auto-relaxed base pins (unavailable on PyPI): {:?}", n));
                    }
                }
                return (py.to_string_lossy().to_string(), String::new());
            }
            let log_text = std::fs::read_to_string(&log).unwrap_or_default();
            let bad = parse_unsatisfiable(&log_text);
            let conflicts = parse_conflict_pins(&log_text, &base_pkgs);
            let new_relax: HashSet<String> =
                bad.union(&conflicts).cloned().collect::<HashSet<_>>().difference(&relax).cloned().collect();
            if new_relax.is_empty() {
                // nothing NEW to relax → a genuine failure; classify it like the Python tool
                if let Some(syslib) = scan_missing_system_dep(&log_text) {
                    return (String::new(), format!("missing system library: {} (see {}.install.log)", syslib, key));
                }
                let low = log_text.to_lowercase();
                if low.contains("resolutionimpossible") || low.contains("conflicting dependencies") {
                    return (String::new(), format!("dependency conflict (see {}.install.log)", key));
                }
                if low.contains("resolution-too-deep") {
                    return (String::new(), format!("pip resolution too deep — needs tighter constraints (see {}.install.log)", key));
                }
                if low.contains("no module named 'pkg_resources'") {
                    return (String::new(), format!("build needs setuptools/pkg_resources (see {}.install.log)", key));
                }
                let note = if relax.is_empty() { String::new() } else {
                    let mut r: Vec<_> = relax.iter().cloned().collect(); r.sort();
                    format!(" after auto-relaxing {:?}", r)
                };
                return (String::new(), format!("pip install failed{} (see {}.install.log)", note, key));
            }
            for c in &conflicts { conflict_relax.insert(c.clone()); }
            for r in new_relax { relax.insert(r); }
        }
        let mut r: Vec<_> = relax.iter().cloned().collect(); r.sort();
        (String::new(), format!("pip install failed even after auto-relaxing {:?} (see {}.install.log)", r, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkg_name_and_normalize() {
        assert_eq!(req_pkg_name("Compreffor==0.5.0  # pin"), "compreffor");
        assert_eq!(req_pkg_name("fontbakery[googlefonts]==0.12"), "fontbakery");
        assert_eq!(req_pkg_name("# comment"), "");
        assert_eq!(req_pkg_name("-r requirements.in"), "");
        assert_eq!(req_pkg_name("git+https://x/y"), "");
        assert_eq!(normalize_requirements("b==2\n# c\n a==1 \n"), "a==1\nb==2");
    }
    #[test]
    fn cohort_key_stable_and_base() {
        assert_eq!(cohort_key_for("   \n# only comments\n"), "base");
        let k1 = cohort_key_for("gftools==0.9.99\ncompreffor==0.5.0");
        let k2 = cohort_key_for("compreffor==0.5.0\ngftools==0.9.99"); // order-independent (normalized)
        assert_eq!(k1, k2);
        assert!(k1.starts_with("c-") && k1.len() == 14);
    }
    #[test]
    fn pin_overrides_and_relax() {
        let lines: Vec<String> = ["gftools==0.9.99", "compreffor==0.5.0", "fontbakery[googlefonts]==0.12"]
            .iter().map(|s| s.to_string()).collect();
        let (out, applied) = apply_pin_overrides(&lines);
        assert!(out[1].starts_with("compreffor>=0.5.6"));
        assert!(out[2].starts_with("fontbakery "));
        assert_eq!(applied, vec!["compreffor", "fontbakery"]);
        let relaxed: HashSet<String> = ["gftools".to_string()].into_iter().collect();
        assert_eq!(relax_requirements(&lines, &relaxed)[0].split_whitespace().next().unwrap(), "gftools");
    }
    #[test]
    fn parsers() {
        assert!(parse_unsatisfiable("ERROR: No matching distribution found for compreffor==0.5.0")
            .contains("compreffor"));
        let base: HashSet<String> = ["gftools".to_string()].into_iter().collect();
        assert!(parse_conflict_pins("ResolutionImpossible\nThe user requested gftools==0.9.99", &base)
            .contains("gftools"));
        assert!(parse_conflict_pins("normal failure", &base).is_empty());
        assert!(scan_missing_system_dep("meson.build:1: Dependency \"cairo\" not found")
            .unwrap().contains("libcairo2-dev"));
    }
}
