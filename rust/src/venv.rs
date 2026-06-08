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
//!
//! Cohort-signature normalization (applied when reading a family's requirements, so it flows to the
//! signature, the install, AND the UI uniformly):
//!   * `-r FILE` / `--requirement FILE` includes are INLINED (read from the mirror at the same commit,
//!     recursively) — a bare `-r requirements.in` is a pointer, not a definition, so the referenced
//!     file's contents are the real requirements.
//!   * QA-only tools are FILTERED OUT (`fontbakery`/`fontspector` dropped; the `[qa]` extra stripped
//!     from any package such as `gftools[qa]`). They don't affect the build, only QA — so two cohorts
//!     differing only in QA tooling collapse into one, and the install avoids fontbakery's heavy and
//!     conflict-prone dependency closure.
//! In the install, a family's own pin OVERRIDES the matching base-toolchain pin (the base only fills
//! in tools the family didn't specify) — this removes the #1 cause of ResolutionImpossible.

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

// ---------- cohort-signature normalization: QA filtering + `-r` include expansion ----------

/// QA-only tools dropped from cohort requirements entirely (they don't affect the build).
const QA_PKGS: [&str; 2] = ["fontbakery", "fontspector"];

/// True for a package that is QA-only (build-irrelevant) and should be dropped from a cohort.
pub fn is_qa_pkg(pkg: &str) -> bool {
    QA_PKGS.contains(&pkg)
}

/// Normalize ONE requirement line for cohort purposes: drop QA-only packages, strip the `[qa]` extra
/// from any package (e.g. `gftools[qa]==X` → `gftools==X`). Returns None if the whole line is dropped.
fn strip_qa_line(line: &str) -> Option<String> {
    let pkg = req_pkg_name(line);
    if pkg.is_empty() {
        return Some(line.to_string()); // blank/comment/option/URL — leave untouched
    }
    if is_qa_pkg(&pkg) {
        return None; // fontbakery / fontspector → dropped entirely
    }
    // strip a `[extra,...]` group, removing any `qa`; drop the brackets if nothing remains
    if let (Some(lb), Some(rb)) = (line.find('['), line.find(']')) {
        if lb < rb {
            let kept: Vec<&str> = line[lb + 1..rb]
                .split(',')
                .map(|e| e.trim())
                .filter(|e| !e.is_empty() && !e.eq_ignore_ascii_case("qa"))
                .collect();
            let rebuilt = if kept.is_empty() {
                format!("{}{}", &line[..lb], &line[rb + 1..])
            } else {
                format!("{}[{}]{}", &line[..lb], kept.join(","), &line[rb + 1..])
            };
            return Some(rebuilt);
        }
    }
    Some(line.to_string())
}

/// Drop QA-only tools and strip `[qa]` extras across a set of requirement lines.
pub fn filter_qa_requirements(lines: &[String]) -> Vec<String> {
    lines.iter().filter_map(|l| strip_qa_line(l)).collect()
}

fn filter_qa_text(text: &str) -> String {
    let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    filter_qa_requirements(&lines).join("\n")
}

/// Assemble the REQUESTED requirement lines for a cohort (before pin-overrides / self-heal relaxation):
/// the QA-filtered base toolchain MINUS any base pin the family itself pins (family wins), followed by
/// the family's own already-(include-expanded + QA-filtered) lines. This is exactly the set `create()`
/// hands the installer — exposed so tooling/tests can reproduce a cohort's effective requirements.
pub fn assemble_requested(base_lines: &[String], req_text: &str, key: &str) -> Vec<String> {
    let cohort_lines: Vec<String> =
        if key == "base" { Vec::new() } else { req_text.lines().map(|s| s.to_string()).collect() };
    let cohort_pkgs: HashSet<String> =
        cohort_lines.iter().map(|l| req_pkg_name(l)).filter(|p| !p.is_empty()).collect();
    let base_kept: Vec<String> = base_lines
        .iter()
        .filter(|l| {
            let p = req_pkg_name(l);
            p.is_empty() || !cohort_pkgs.contains(&p)
        })
        .cloned()
        .collect();
    base_kept.iter().chain(cohort_lines.iter()).cloned().collect()
}

/// The file referenced by a `-r FILE` / `--requirement FILE` include directive, if this line is one.
fn include_target(line: &str) -> Option<String> {
    let s = line.trim();
    for pre in ["--requirement=", "-r=", "--requirement ", "-r "] {
        if let Some(rest) = s.strip_prefix(pre) {
            let f = rest.split('#').next().unwrap_or("").trim();
            if !f.is_empty() {
                return Some(f.to_string());
            }
        }
    }
    None
}

/// Resolve an include path relative to the including file's directory, collapsing `.`/`..`.
fn normalize_join(base_dir: &str, target: &str) -> String {
    let combined = if target.starts_with('/') {
        target.trim_start_matches('/').to_string()
    } else {
        format!("{}{}", base_dir, target)
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in combined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Inline every `-r FILE` include by calling `read(path)` (recursively, cycle-guarded). A `-r` line is
/// a pointer to the real requirements, so we splice the referenced file's contents in its place.
fn expand_includes<R: Fn(&str) -> Option<String>>(
    read: &R,
    from: &str,
    text: &str,
    seen: &mut HashSet<String>,
    depth: usize,
) -> String {
    if depth > 8 {
        return text.to_string();
    }
    let base_dir = from.rfind('/').map(|i| &from[..i + 1]).unwrap_or("");
    let mut out: Vec<String> = Vec::new();
    for ln in text.lines() {
        match include_target(ln) {
            Some(target) => {
                let path = normalize_join(base_dir, &target);
                if !seen.insert(path.clone()) {
                    continue; // already inlined — skip the cycle
                }
                match read(&path) {
                    Some(inc) => {
                        out.push(format!("# gflib-build: inlined -r {}", path));
                        out.push(expand_includes(read, &path, &inc, seen, depth + 1));
                    }
                    // unreadable include → keep the literal line so the signature still reflects it
                    None => out.push(ln.to_string()),
                }
            }
            None => out.push(ln.to_string()),
        }
    }
    out.join("\n")
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

/// Packages whose wheel BUILD failed (an sdist that won't compile here). Relaxing their pin lets pip
/// fall back to a newer version that ships a binary wheel — no build at all. This is the elegant fix
/// for stale pins like `openstep-plist==0.3.1` (no cp313 wheel) on a newer Python: prefer the wheel
/// rather than blanket-pinning the build toolchain.
pub fn parse_failed_wheel_builds(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    // (1) the explicit summary forms pip prints when a wheel BUILD (not metadata) fails.
    for marker in ["Failed building wheel for ", "Failed to build "] {
        let mut rest = text;
        while let Some(pos) = rest.find(marker) {
            let after = &rest[pos + marker.len()..];
            if after.starts_with("installable wheels") {
                // "Failed to build installable wheels for some pyproject.toml based projects (a, b)"
                if let Some(lp) = after.find('(') {
                    if let Some(rp) = after[lp..].find(')') {
                        for p in after[lp + 1..lp + rp].split(',') {
                            let tok = take_pkg(p.trim().trim_start_matches(['\'', '"']));
                            if !tok.is_empty() {
                                out.insert(tok.to_lowercase());
                            }
                        }
                    }
                }
            } else {
                // "Failed building wheel for X" / "Failed to build 'X'" / "Failed to build X"
                let tok = take_pkg(after.trim_start_matches(['\'', '"']));
                if !tok.is_empty() {
                    out.insert(tok.to_lowercase());
                }
            }
            rest = &after[1.min(after.len())..];
        }
    }
    // (2) Modern pip (24+) fails an sdist during "Getting requirements to build wheel" with a bare
    //     `subprocess-exited-with-error` and names NO package in a summary line. The failing package is
    //     the most recent `Collecting <pkg>` before the error (pip builds an sdist's metadata right
    //     after collecting it); a package's own `Building <pkg> version …` banner names it directly too.
    let mut last_collecting: Option<String> = None;
    for ln in text.lines() {
        let t = ln.trim();
        if let Some(rest) = t.strip_prefix("Collecting ") {
            let tok = take_pkg(rest);
            if !tok.is_empty() {
                last_collecting = Some(tok.to_lowercase());
            }
        } else if let Some(rest) = t.strip_prefix("Building ") {
            if rest.contains(" version ") {
                // "Building lxml version 5.2.1." (not "Building wheel for X")
                let tok = take_pkg(rest);
                if !tok.is_empty() {
                    out.insert(tok.to_lowercase());
                }
            }
        } else if t.contains("did not run successfully") || t.contains("subprocess-exited-with-error") {
            if let Some(p) = &last_collecting {
                out.insert(p.clone());
            }
        }
    }
    out
}

/// Packages pip is BUILDING from an sdist (its `Collecting <pkg>` is followed by a `.tar.gz`/`.zip`
/// download, not a wheel). On a build failure we relax ALL of them in one pass so pip backtracks to
/// versions that ship a binary wheel — the pre-py3.13 freeze pins dozens of pre-cp313 versions, and
/// relaxing one-per-attempt is too slow (and risks an interrupted run on this fd-constrained box).
pub fn parse_sdist_packages(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut pending: Option<String> = None;
    for ln in text.lines() {
        let t = ln.trim();
        if let Some(rest) = t.strip_prefix("Collecting ") {
            let tok = take_pkg(rest);
            pending = if tok.is_empty() { None } else { Some(tok.to_lowercase()) };
        } else if t.starts_with("Using cached ") || t.starts_with("Downloading ") {
            if t.contains(".tar.gz") || t.contains(".zip") {
                if let Some(p) = pending.take() {
                    out.insert(p);
                }
            } else {
                pending = None; // a wheel — not a build candidate
            }
        }
    }
    out
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

/// `git show <commit>:<path>` on a bare mirror, or None if the path is absent at that commit.
fn git_show(mirror: &Path, commit: &str, path: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["--git-dir", &mirror.to_string_lossy(), "show", &format!("{}:{}", commit, path)])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        None
    }
}

/// Read a repo's requirements at a commit WITHOUT extracting — read-only `git show` on the mirror.
/// `-r` includes are inlined (read from the mirror) and QA-only tools filtered, so the returned text
/// is the canonical cohort requirements (drives signature, install, and the UI alike).
pub fn read_requirements_from_mirror(mirror: &Path, commit: &str) -> String {
    let read = |p: &str| git_show(mirror, commit, p);
    for r in REQ_FILES {
        if let Some(raw) = read(r) {
            let expanded = expand_includes(&read, r, &raw, &mut HashSet::new(), 0);
            return filter_qa_text(&expanded);
        }
    }
    String::new()
}

/// Read the family's requirements from its extracted work tree (post-checkout). Parity API; the build
/// path reads from the mirror (read-only) instead, but `--cohorts-report` / pre-build will use this.
/// Applies the SAME include expansion + QA filtering so cohort keys agree with the mirror reader.
#[allow(dead_code)]
pub fn read_requirements(work: &Path) -> String {
    let read = |p: &str| std::fs::read_to_string(work.join(p)).ok();
    for r in REQ_FILES {
        if work.join(r).is_file() {
            if let Some(raw) = read(r) {
                let expanded = expand_includes(&read, r, &raw, &mut HashSet::new(), 0);
                return filter_qa_text(&expanded);
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
        let base_lines: Vec<String> = filter_qa_requirements(
            &self
                .base_req
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| t.lines().map(|s| s.to_string()).collect::<Vec<_>>())
                .unwrap_or_default(),
        );
        // req_text arriving from the mirror reader is already include-expanded + QA-filtered.
        // family pins WIN: assemble_requested drops any base toolchain pin the cohort also pins, so the
        // upstream repo's declared version is honored instead of colliding with the base pin (the #1
        // cause of ResolutionImpossible). Base only supplies tools the family didn't specify.
        let requested: Vec<String> = assemble_requested(&base_lines, req_text, key);
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
        // Seed setuptools<81 + wheel. setuptools 81 deprecated and 82 REMOVED pkg_resources, but this
        // pinned font toolchain (gftools/fontmake + many deps) imports pkg_resources at BOTH build time
        // (legacy sdists) and RUNTIME (`python -m gftools.builder` won't even import otherwise).
        // setuptools' own warning says to "pin to Setuptools<81", so this is a real toolchain requirement.
        let _ = Command::new(&py)
            .args(["-m", "pip", "install", "-q", "--disable-pip-version-check", "--cache-dir",
                   &self.pip_cache.to_string_lossy(), "setuptools<81", "wheel"])
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
        // Constrain setuptools<81 for the whole install INCLUDING pip's isolated build envs (via
        // PIP_CONSTRAINT). The pinned toolchain (gftools/fontmake + deps) imports pkg_resources, which
        // setuptools 82 removed; setuptools' own warning says to "pin to Setuptools<81". So it's a real
        // toolchain requirement, applied uniformly — not a per-cohort workaround.
        let con_path = vdir.join("gflib-constraints.txt");
        let _ = std::fs::write(&con_path, "setuptools<81\n");
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
                    let mut cmd = Command::new(&py);
                    cmd.args(["-m", "pip", "install", "--disable-pip-version-check", "--cache-dir",
                              &self.pip_cache.to_string_lossy(), "-r", &eff_path.to_string_lossy()]);
                    cmd.env("PIP_CONSTRAINT", &con_path); // setuptools<81 in the venv AND build envs
                    cmd.stdout(Stdio::from(lf))
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
            // a stale pin whose sdist won't build here → relax it so pip takes a wheel version instead.
            // When ANY build fails, relax EVERY sdist-built package at once (the pre-py3.13 freeze pins
            // many pre-cp313 versions) so we converge in ~1 retry rather than one-per-attempt.
            let failed_builds = parse_failed_wheel_builds(&log_text);
            let sdist = if failed_builds.is_empty() {
                HashSet::new()
            } else {
                parse_sdist_packages(&log_text)
            };
            let mut candidates: HashSet<String> = bad.union(&conflicts).cloned().collect();
            candidates.extend(failed_builds.iter().cloned());
            candidates.extend(sdist.iter().cloned());
            let new_relax: HashSet<String> = candidates.difference(&relax).cloned().collect();
            // record (once) which build-failing pins we dropped, for the dashboard's relaxations list
            let fresh_failed: Vec<String> = failed_builds.difference(&relax).cloned().collect();
            if !fresh_failed.is_empty() {
                let mut inner = self.inner.lock().unwrap();
                for p in &fresh_failed {
                    if inner.override_recorded.insert(format!("build:{}", p)) {
                        inner.relaxations.push(format!(
                            "relaxed {} pin — its sdist won't build here; pip will use a wheel version", p));
                    }
                }
            }
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
    fn parse_failed_wheel_builds_extracts_pkgs() {
        // explicit summary forms pip prints when a wheel BUILD fails
        let log = "ERROR: Failed building wheel for openstep-plist\n\
                   ERROR: Failed to build 'cu2qu' when getting requirements to build wheel\n\
                   ERROR: Failed to build installable wheels for some pyproject.toml based projects (skia-pathops, compreffor)\n";
        let got = parse_failed_wheel_builds(log);
        for p in ["openstep-plist", "cu2qu", "skia-pathops", "compreffor"] {
            assert!(got.contains(p), "expected {} in {:?}", p, got);
        }
        // modern pip (24+) names NO package in a summary — attribute to the preceding Collecting and
        // the Building banner; a package installed as a WHEEL (jinja2) must NOT be relaxed.
        let modern = "Collecting Jinja2==3.1.3\n  Using cached Jinja2-3.1.3-py3-none-any.whl\n\
                      Collecting lxml==5.2.1\n  Using cached lxml-5.2.1.tar.gz\n\
                      Getting requirements to build wheel: finished with status 'error'\n\
                      error: subprocess-exited-with-error\n\
                      Building lxml version 5.2.1.\n\
                      x Getting requirements to build wheel did not run successfully.\n";
        let m = parse_failed_wheel_builds(modern);
        assert!(m.contains("lxml"), "expected lxml in {:?}", m);
        assert!(!m.contains("jinja2"), "a wheel package must NOT be relaxed: {:?}", m);
        assert!(parse_failed_wheel_builds("Successfully installed everything").is_empty());
    }
    #[test]
    fn parse_sdist_packages_finds_build_candidates() {
        let log = "Collecting Jinja2==3.1.3\n  Using cached Jinja2-3.1.3-py3-none-any.whl\n\
                   Collecting lxml==5.2.1\n  Using cached lxml-5.2.1.tar.gz (3.7 MB)\n\
                   Collecting numpy==1.26\n  Downloading numpy-1.26.tar.gz\n";
        let s = parse_sdist_packages(log);
        assert!(s.contains("lxml") && s.contains("numpy"), "sdists: {:?}", s);
        assert!(!s.contains("jinja2"), "a wheel is not a build candidate: {:?}", s);
    }
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
    fn reuses_a_venv_with_a_matching_marker() {
        // The drop-in property: a venv whose .gflib-installed marker matches the requirements is
        // returned as-is — never rebuilt. (Proven offline: no real `python -m venv` / pip needed.)
        let bd = std::env::temp_dir().join(format!("_vmreuse_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bd);
        let basereq = bd.join("base.txt");
        std::fs::create_dir_all(&bd).unwrap();
        std::fs::write(&basereq, "wheel\n").unwrap();
        // pre-stage a "ready" base venv: a dummy bin/python + the correct marker hash
        let vdir = bd.join("venvs").join("base");
        std::fs::create_dir_all(vdir.join("bin")).unwrap();
        let dummy_py = vdir.join("bin").join("python");
        std::fs::write(&dummy_py, "#!/bin/sh\n").unwrap();
        let want = sha_hex("sha256sum", "wheel"); // base cohort, requested = ["wheel"], no override
        std::fs::write(vdir.join(".gflib-installed"), format!("{}\n", &want[..want.len().min(16)])).unwrap();

        let vm = VenvManager::new(&bd, "python3", Some(basereq));
        // on_install is a "starting" notification (called before create() regardless of reuse). The
        // real reuse signal: create() returns early WITHOUT rmtree, so the dummy file is untouched.
        let (py, key, err) = vm.get_python("", |_| {});
        assert_eq!(key, "base");
        assert!(err.is_empty(), "reuse should not error: {}", err);
        assert_eq!(py, dummy_py.to_string_lossy(), "must return the existing venv's python");
        assert_eq!(
            std::fs::read_to_string(&dummy_py).unwrap(),
            "#!/bin/sh\n",
            "the existing venv must be left intact (a rebuild would rmtree + recreate it)"
        );
        let _ = std::fs::remove_dir_all(&bd);
    }

    #[test]
    fn qa_filtering_drops_qa_tools_and_strips_qa_extra() {
        let lines: Vec<String> = [
            "gftools[qa]==0.9.99",
            "fontbakery[googlefonts]==0.12",
            "fontspector",
            "fontmake==3.11.1",
            "gftools[ci,qa]>=0.9",
            "# a comment",
            "-e .",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = filter_qa_requirements(&lines);
        assert_eq!(
            out,
            vec!["gftools==0.9.99", "fontmake==3.11.1", "gftools[ci]>=0.9", "# a comment", "-e ."]
        );
    }

    #[test]
    fn family_pins_override_base_toolchain() {
        let base: Vec<String> = ["fontmake==3.11.1", "fonttools==4.61.1", "gftools==0.9.99"]
            .iter().map(|s| s.to_string()).collect();
        // family pins fontmake + gftools (looser); base fontmake/gftools are DROPPED, fonttools kept
        let got = assemble_requested(&base, "fontmake>=2.4\ngftools>=0.7\ndrawbot-skia>=0.4.8", "c-x");
        assert_eq!(
            got,
            vec!["fonttools==4.61.1", "fontmake>=2.4", "gftools>=0.7", "drawbot-skia>=0.4.8"]
        );
        // the 'base' cohort itself takes no family lines
        assert_eq!(assemble_requested(&base, "whatever", "base"), base);
    }

    #[test]
    fn cohort_merges_when_only_qa_differs() {
        // two families whose ONLY difference is QA tooling must land in the SAME cohort
        let a = filter_qa_text("gftools==0.9.99\nfontmake==3.11.1\nfontbakery[googlefonts]==0.12\n");
        let b = filter_qa_text("gftools[qa]==0.9.99\nfontmake==3.11.1\n");
        assert_eq!(cohort_key_for(&a), cohort_key_for(&b));
        // and a family whose only requirement was a QA tool collapses into the base cohort
        assert_eq!(cohort_key_for(&filter_qa_text("fontbakery\n")), "base");
    }

    #[test]
    fn include_directive_parsing_and_join() {
        assert_eq!(include_target("-r requirements.in").as_deref(), Some("requirements.in"));
        assert_eq!(include_target("--requirement=base.txt # note").as_deref(), Some("base.txt"));
        assert_eq!(include_target("  -r  ../shared/req.txt").as_deref(), Some("../shared/req.txt"));
        assert_eq!(include_target("gftools==0.9.99"), None);
        assert_eq!(include_target("-e ."), None);
        assert_eq!(normalize_join("sources/", "../requirements.in"), "requirements.in");
        assert_eq!(normalize_join("", "a/./b.txt"), "a/b.txt");
    }

    #[test]
    fn expand_includes_inlines_referenced_files() {
        // a fake mini "repo": requirements.txt is just `-r requirements.in`, which holds the real deps
        let files: HashMap<&str, &str> = [
            ("requirements.txt", "-r requirements.in\nfontbakery\n"),
            ("requirements.in", "gftools==0.9.99\nfontmake==3.11.1\n"),
        ]
        .into_iter()
        .collect();
        let read = |p: &str| files.get(p).map(|s| s.to_string());
        let raw = read("requirements.txt").unwrap();
        let expanded = expand_includes(&read, "requirements.txt", &raw, &mut HashSet::new(), 0);
        let filtered = filter_qa_text(&expanded);
        // the include is inlined and the QA tool dropped → cohort of just the real toolchain pins
        assert_eq!(cohort_key_for(&filtered), cohort_key_for("gftools==0.9.99\nfontmake==3.11.1"));
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
