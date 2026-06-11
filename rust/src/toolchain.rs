//! Zero-setup, guaranteed-available Rust toolchain: fontc (compiler) + gftools-builder3
//! (orchestrator). Neither can be a literal Cargo dependency — fontc is binary-only (no lib
//! target) and builder3 carries git dependencies (unpublishable to crates.io) — so "guaranteed"
//! is honored by PINNED AUTO-PROVISIONING: when a tool isn't found, the build provisions the
//! pinned release itself via `cargo install` into `<data-dir>/tools/<name>-<pin>/` (the same
//! pattern fontspector.rs already uses), records the exact version in M0 provenance, and the
//! run degrades gracefully (builder2 / fontmake) if provisioning fails. cargo is assumed
//! present: whoever built gflib-build has it.
//!
//! Pin-bump procedure: change FONTC_VERSION / BUILDER3_REV below; the next run provisions the
//! new pin into a new version-keyed dir (old installs are left in place and never deleted).
//!
//! Resolution order per tool (first hit wins):
//!   1. the explicit CLI/config override (--fontc-bin / --builder3-bin) — always wins;
//!   2. the provisioned pin at <data-dir>/tools/<name>-<pin>/bin/<bin> (cached install);
//!   3. auto-provision the pin (default on; --no-toolchain-provision disables);
//!   4. a detected binary (PATH / sibling checkouts) — fallback only, so a stale local build
//!      never silently shadows the pin.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};

/// Pinned fontc release, installed from crates.io (`cargo install fontc --version …`); falls back
/// to the matching git tag if the registry fetch fails. Matches the fontc embedded in the pinned
/// builder3, so builder3 and builder2+fontc attempts compile with the same fontc.
pub const FONTC_VERSION: &str = "0.6.0";
pub const FONTC_GIT: &str = "https://github.com/googlefonts/fontc";

/// Pinned gftools-builder3 revision (no crates.io release exists — git deps), installed with
/// `cargo install --git … --rev … --locked` using the repo's committed Cargo.lock.
pub const BUILDER3_GIT: &str = "https://github.com/simoncozens/gftools-builder3";
pub const BUILDER3_REV: &str = "cf74f20a995a9cff78e1a9e3cd8303caf0ae25d4";
/// builder3's package + binary name (its Cargo.toml: package "gftools-builder", version 3.x).
pub const BUILDER3_PKG: &str = "gftools-builder";

/// How a tool's binary was obtained — recorded in the control log and the config tab.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolStatus {
    /// Resolution has not finished yet (workers wait on the gate).
    Pending,
    /// Usable binary. `source` is one of "flag" | "provisioned" | "detected".
    Ready { path: String, source: &'static str },
    /// Nothing usable and provisioning failed/disabled — the chain degrades past this tool.
    Unavailable(String),
}

/// The two-tool ready-gate. The resolver thread (spawned at Orchestrator start) fills it in;
/// build workers block on `wait()` until both tools have a verdict. The whole point is that a
/// verdict always arrives: Ready or Unavailable, never a hang.
pub struct Toolchain {
    state: Mutex<(ToolStatus, ToolStatus)>, // (fontc, builder3)
    cv: Condvar,
}

impl Default for Toolchain {
    fn default() -> Self {
        Toolchain { state: Mutex::new((ToolStatus::Pending, ToolStatus::Pending)), cv: Condvar::new() }
    }
}

impl Toolchain {
    /// Block until both tools are resolved; returns (fontc_bin, builder3_bin).
    pub fn wait(&self) -> (Option<String>, Option<String>) {
        let mut g = self.state.lock().unwrap();
        while matches!(g.0, ToolStatus::Pending) || matches!(g.1, ToolStatus::Pending) {
            g = self.cv.wait(g).unwrap();
        }
        (path_of(&g.0), path_of(&g.1))
    }

    /// Non-blocking view for snapshots / task rows.
    pub fn peek(&self) -> (ToolStatus, ToolStatus) {
        let g = self.state.lock().unwrap();
        (g.0.clone(), g.1.clone())
    }

    pub fn set_fontc(&self, s: ToolStatus) {
        self.state.lock().unwrap().0 = s;
        self.cv.notify_all();
    }

    pub fn set_builder3(&self, s: ToolStatus) {
        self.state.lock().unwrap().1 = s;
        self.cv.notify_all();
    }

}

fn path_of(s: &ToolStatus) -> Option<String> {
    match s {
        ToolStatus::Ready { path, .. } => Some(path.clone()),
        _ => None,
    }
}

/// What to install and from where. Split out of the resolution flow so the provisioner is
/// unit-testable against a tiny fixture crate (file:// git URL) without touching the network.
pub struct ToolSpec {
    pub name: &'static str,         // dir prefix under tools/ ("fontc" | "builder3")
    pub bin_name: &'static str,     // the installed binary's file name
    pub pin: String,                // version or short rev — keys the install dir
    pub install: InstallSource,
}

pub enum InstallSource {
    /// crates.io release; optional (git_url, git_ref) fallback if the registry install fails.
    Registry { krate: &'static str, version: String, git_fallback: Option<(String, String)> },
    /// git repo at an exact rev (uses the repo's committed Cargo.lock via --locked).
    Git { url: String, rev: String, package: &'static str },
}

pub fn fontc_spec() -> ToolSpec {
    ToolSpec {
        name: "fontc",
        bin_name: "fontc",
        pin: FONTC_VERSION.into(),
        install: InstallSource::Registry {
            krate: "fontc",
            version: FONTC_VERSION.into(),
            git_fallback: Some((FONTC_GIT.into(), format!("fontc-v{}", FONTC_VERSION))),
        },
    }
}

pub fn builder3_spec() -> ToolSpec {
    ToolSpec {
        name: "builder3",
        bin_name: BUILDER3_PKG,
        pin: BUILDER3_REV[..10.min(BUILDER3_REV.len())].into(),
        install: InstallSource::Git { url: BUILDER3_GIT.into(), rev: BUILDER3_REV.into(), package: BUILDER3_PKG },
    }
}

/// The provisioned location for a spec: <tools_root>/<name>-<pin>/bin/<bin_name>.
pub fn provisioned_bin(tools_root: &Path, spec: &ToolSpec) -> PathBuf {
    tools_root.join(format!("{}-{}", spec.name, spec.pin)).join("bin").join(spec.bin_name)
}

/// Resolve one tool: explicit override → cached pin → provision → detect. Pure with respect to
/// its inputs (tools_root injectable) so tests drive it end-to-end with a fixture crate.
/// `detect` supplies the step-4 fallback (PATH/sibling probes), run only when earlier steps miss.
pub fn ensure_tool(
    spec: &ToolSpec,
    explicit: Option<&str>,
    tools_root: &Path,
    auto_provision: bool,
    detect: impl Fn() -> Option<String>,
) -> ToolStatus {
    // 1. explicit flag/config — trusted as-is (the user's word beats our pin)
    if let Some(b) = explicit {
        if is_executable(Path::new(b)) {
            return ToolStatus::Ready { path: b.to_string(), source: "flag" };
        }
        return ToolStatus::Unavailable(format!("{}: explicit binary not executable: {}", spec.name, b));
    }
    // 2. already-provisioned pin
    let pin_bin = provisioned_bin(tools_root, spec);
    if pin_bin.is_file() {
        return ToolStatus::Ready { path: pin_bin.to_string_lossy().into_owned(), source: "provisioned" };
    }
    // 3. provision the pin
    if auto_provision {
        match provision(spec, tools_root) {
            Ok(p) => return ToolStatus::Ready { path: p.to_string_lossy().into_owned(), source: "provisioned" },
            Err(e) => {
                // fall through to detection — a local binary is better than nothing
                if let Some(d) = detect() {
                    return ToolStatus::Ready { path: d, source: "detected" };
                }
                return ToolStatus::Unavailable(e);
            }
        }
    }
    // 4. detection fallback (provisioning disabled)
    if let Some(d) = detect() {
        return ToolStatus::Ready { path: d, source: "detected" };
    }
    ToolStatus::Unavailable(format!(
        "{}: not found and auto-provisioning is disabled (--toolchain-provision to enable, or pass an explicit binary)",
        spec.name
    ))
}

/// `cargo install` the pinned tool into its version-keyed root. Output is captured to
/// <tools_root>/provision-<name>.log so a failure is debuggable.
fn provision(spec: &ToolSpec, tools_root: &Path) -> Result<PathBuf, String> {
    let root = tools_root.join(format!("{}-{}", spec.name, spec.pin));
    let _ = std::fs::create_dir_all(tools_root);
    let log = tools_root.join(format!("provision-{}.log", spec.name));

    let attempts: Vec<Vec<String>> = match &spec.install {
        InstallSource::Registry { krate, version, git_fallback } => {
            let mut v = vec![vec![
                "install".into(), krate.to_string(),
                "--version".into(), version.clone(),
                "--locked".into(),
            ]];
            if let Some((url, gref)) = git_fallback {
                v.push(vec![
                    "install".into(),
                    "--git".into(), url.clone(),
                    "--tag".into(), gref.clone(),
                    "--locked".into(),
                    krate.to_string(),
                ]);
            }
            v
        }
        InstallSource::Git { url, rev, package } => vec![vec![
            "install".into(),
            "--git".into(), url.clone(),
            "--rev".into(), rev.clone(),
            "--locked".into(),
            package.to_string(),
        ]],
    };

    let mut last = String::new();
    for args in &attempts {
        let logf = std::fs::OpenOptions::new().create(true).append(true).open(&log)
            .map_err(|e| format!("{}: open provision log: {}", spec.name, e))?;
        let logf2 = logf.try_clone().map_err(|e| format!("{}: log fd: {}", spec.name, e))?;
        let status = Command::new("cargo")
            .args(args)
            .arg("--root").arg(&root)
            .stdout(std::process::Stdio::from(logf))
            .stderr(std::process::Stdio::from(logf2))
            .status()
            .map_err(|e| format!("{}: could not run cargo (is cargo on PATH?): {}", spec.name, e))?;
        let bin = provisioned_bin(tools_root, spec);
        if status.success() && bin.is_file() {
            return Ok(bin);
        }
        last = format!("{}: cargo install failed (rc={}) — see {}", spec.name, status, log.display());
    }
    Err(last)
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file() && p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Detection fallback for builder3 (the analogue of discover::detect_fontc): sibling checkouts
/// first, then PATH. A checkout path (…/gftools-builder3/target/release/…) is unambiguous, so
/// it's accepted on existence — builder3 builds that predate `--version` support still detect.
/// A bare PATH hit must IDENTIFY as builder3 (major version 3), because the Python gftools also
/// ships a `gftools-builder` console script that would otherwise shadow it.
pub fn detect_builder3() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let cands = [
        format!("../gftools-builder3/target/release/{}", BUILDER3_PKG),
        format!("{}/gftools-builder3/target/release/{}", home, BUILDER3_PKG),
        format!("gftools-builder3/target/release/{}", BUILDER3_PKG),
    ];
    for c in &cands {
        let p = Path::new(c);
        if p.is_file() {
            return std::fs::canonicalize(p).ok().map(|p| p.to_string_lossy().into_owned());
        }
    }
    if let Ok(o) = Command::new("sh").args(["-c", &format!("command -v {}", BUILDER3_PKG)]).output() {
        let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !p.is_empty() && is_builder3_binary(Path::new(&p)) {
            return Some(p);
        }
    }
    None
}

/// True when `--version` output looks like builder3 (3.x), not the Python builder2 shim.
fn is_builder3_binary(p: &Path) -> bool {
    if let Ok(o) = Command::new(p).arg("--version").output() {
        let txt = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        let line = txt.lines().next().unwrap_or("");
        // e.g. "gftools-builder 3.0.0" — accept any 3.x; reject gftools(-builder2) 0.x
        return line.contains("gftools-builder") && line.split_whitespace().any(|w| w.starts_with('3'));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gflib-toolchain-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn explicit_flag_wins_and_must_be_executable() {
        let d = tmpdir("flag");
        let spec = fontc_spec();
        // a non-executable explicit path is an error, not a silent fallback
        let f = d.join("notabin");
        std::fs::write(&f, "x").unwrap();
        let st = ensure_tool(&spec, Some(f.to_str().unwrap()), &d, false, || None);
        assert!(matches!(st, ToolStatus::Unavailable(_)));
        // an executable explicit path is taken verbatim, never re-provisioned
        let sh = d.join("fakebin");
        std::fs::write(&sh, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
        match ensure_tool(&spec, Some(sh.to_str().unwrap()), &d, false, || None) {
            ToolStatus::Ready { source, .. } => assert_eq!(source, "flag"),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn cached_pin_outranks_detection() {
        let d = tmpdir("pin");
        let spec = builder3_spec();
        let bin = provisioned_bin(&d, &spec);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        // detection would return a decoy — the cached pin must win without calling provision
        match ensure_tool(&spec, None, &d, false, || Some("/decoy/builder".into())) {
            ToolStatus::Ready { path, source } => {
                assert_eq!(source, "provisioned");
                assert!(path.ends_with(&format!("builder3-{}/bin/{}", spec.pin, BUILDER3_PKG)));
            }
            other => panic!("expected provisioned Ready, got {:?}", other),
        }
    }

    #[test]
    fn detection_is_the_fallback_when_provisioning_disabled() {
        let d = tmpdir("detect");
        let spec = fontc_spec();
        match ensure_tool(&spec, None, &d, false, || Some("/detected/fontc".into())) {
            ToolStatus::Ready { path, source } => {
                assert_eq!(source, "detected");
                assert_eq!(path, "/detected/fontc");
            }
            other => panic!("expected detected Ready, got {:?}", other),
        }
        // nothing detected either → Unavailable with an actionable message
        match ensure_tool(&spec, None, &d, false, || None) {
            ToolStatus::Unavailable(msg) => assert!(msg.contains("auto-provisioning is disabled")),
            other => panic!("expected Unavailable, got {:?}", other),
        }
    }

    #[test]
    fn provision_installs_a_fixture_crate_end_to_end() {
        // A real `cargo install --git file://… --rev … --locked` against a tiny hello-world crate:
        // exercises the exact code path used for builder3, in seconds, with no network.
        let d = tmpdir("e2e");
        let src = d.join("fixture-src");
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("Cargo.toml"),
            "[package]\nname = \"gflib-fixture-tool\"\nversion = \"0.0.1\"\nedition = \"2021\"\n[[bin]]\nname = \"gflib-fixture-tool\"\npath = \"src/main.rs\"\n").unwrap();
        std::fs::write(src.join("src/main.rs"), "fn main() { println!(\"gflib-fixture-tool 3.0.0\"); }\n").unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git").args(args).current_dir(&src)
                .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
                .output().map(|o| o.status.success()).unwrap_or(false);
            assert!(ok, "git {:?} failed", args);
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "fixture"]);
        // generate the lockfile (required by --locked) and commit it
        assert!(Command::new("cargo").args(["generate-lockfile"]).current_dir(&src)
            .output().map(|o| o.status.success()).unwrap_or(false));
        git(&["add", "Cargo.lock"]);
        git(&["commit", "-qm", "lock"]);
        let rev = String::from_utf8(
            Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&src).output().unwrap().stdout
        ).unwrap().trim().to_string();

        let spec = ToolSpec {
            name: "fixture",
            bin_name: "gflib-fixture-tool",
            pin: rev[..10].into(),
            install: InstallSource::Git { url: format!("file://{}", src.display()), rev, package: "gflib-fixture-tool" },
        };
        let tools = d.join("tools");
        match ensure_tool(&spec, None, &tools, true, || None) {
            ToolStatus::Ready { path, source } => {
                assert_eq!(source, "provisioned");
                let out = Command::new(&path).output().unwrap();
                assert!(String::from_utf8_lossy(&out.stdout).contains("gflib-fixture-tool"));
                // second resolution must hit the cache (no re-install)
                match ensure_tool(&spec, None, &tools, false, || None) {
                    ToolStatus::Ready { source, .. } => assert_eq!(source, "provisioned"),
                    other => panic!("cache miss: {:?}", other),
                }
            }
            ToolStatus::Unavailable(e) => panic!("provision failed: {}", e),
            ToolStatus::Pending => unreachable!(),
        }
    }

    #[test]
    fn gate_wait_unblocks_on_both_verdicts() {
        use std::sync::Arc;
        let tc = Arc::new(Toolchain::default());
        let t2 = Arc::clone(&tc);
        let h = std::thread::spawn(move || t2.wait());
        std::thread::sleep(std::time::Duration::from_millis(30));
        tc.set_fontc(ToolStatus::Ready { path: "/x/fontc".into(), source: "detected" });
        tc.set_builder3(ToolStatus::Unavailable("nope".into()));
        let (f, b3) = h.join().unwrap();
        assert_eq!(f.as_deref(), Some("/x/fontc"));
        assert!(b3.is_none());
    }
}
