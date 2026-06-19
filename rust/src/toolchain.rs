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
//!   2b. a pin-MATCHED local checkout build (rev/version-verified) — zero-provision, and survives a
//!      flaky cargo install; a wrong rev/version is rejected, so it never shadows the pin;
//!   3. auto-provision the pin (default on; --no-toolchain-provision disables);
//!   4. a detected binary (PATH / sibling checkouts) — fallback only, so a stale local build
//!      never silently shadows the pin.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Hard ceiling on one `cargo install` (a full builder3 build is minutes; a wedged network
/// fetch must not hang the resolver forever).
const PROVISION_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// Ceiling on a `--version` identification probe (an unknown PATH binary must not hang us).
const PROBE_DEADLINE: Duration = Duration::from_secs(10);
/// A provision lock older than this is presumed left by a dead process and is broken.
const LOCK_STALE: Duration = Duration::from_secs(45 * 60);

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
    /// Usable binary. `source` is one of "flag" | "provisioned" | "checkout" | "detected".
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

    /// Force a verdict on anything still Pending — the resolver's drop-guard calls this so a
    /// panicking resolver thread can never strand workers on the gate.
    pub fn resolve_pending(&self, msg: &str) {
        let mut g = self.state.lock().unwrap();
        if matches!(g.0, ToolStatus::Pending) {
            g.0 = ToolStatus::Unavailable(format!("fontc: {}", msg));
        }
        if matches!(g.1, ToolStatus::Pending) {
            g.1 = ToolStatus::Unavailable(format!("builder3: {}", msg));
        }
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
    /// Empirical minimum rustc the pin compiles with (from its lockfile). Checked BEFORE the
    /// install so an old toolchain fails in milliseconds with "run rustup update", not after
    /// minutes of compilation with the cause buried in a 200-line cargo log.
    pub min_rustc: Option<&'static str>,
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
        min_rustc: None, // fontc 0.6.0 verified building on rustc 1.91
    }
}

pub fn builder3_spec() -> ToolSpec {
    ToolSpec {
        name: "builder3",
        bin_name: BUILDER3_PKG,
        pin: BUILDER3_REV[..10.min(BUILDER3_REV.len())].into(),
        install: InstallSource::Git { url: BUILDER3_GIT.into(), rev: BUILDER3_REV.into(), package: BUILDER3_PKG },
        // the cf74f20 lockfile pins ascii-dag 0.4.2, whose rust-version is 1.92 (verified
        // empirically: rustc 1.91.1 fails the install). Re-derive when bumping BUILDER3_REV.
        min_rustc: Some("1.92"),
    }
}

/// The provisioned location for a spec: <tools_root>/<name>-<pin>/bin/<bin_name>.
pub fn provisioned_bin(tools_root: &Path, spec: &ToolSpec) -> PathBuf {
    tools_root.join(format!("{}-{}", spec.name, spec.pin)).join("bin").join(spec.bin_name)
}

/// True if a `--version` output names `version` as a whole TOKEN (optionally `v`-prefixed) rather than a
/// substring — so "fontc 10.6.0" and "fontc 0.6.0-pre" do NOT match the pin "0.6.0" (which a bare
/// `contains` would wrongly accept, silently using a wrong-version build).
fn version_token_matches(output: &str, version: &str) -> bool {
    output
        .split(|c: char| c.is_whitespace())
        .any(|tok| tok == version || tok.trim_start_matches('v') == version)
}

/// The repository directory name implied by a git URL, e.g. ".../simoncozens/gftools-builder3(.git)"
/// → "gftools-builder3". Lets us look for a checkout WITHOUT hardcoding any repo name or path.
fn git_url_repo_name(url: &str) -> String {
    url.trim_end_matches('/').rsplit('/').next().unwrap_or("").trim_end_matches(".git").to_string()
}

/// Already-built binaries to try BEFORE cargo-installing — a pin-matched local checkout makes the
/// toolchain "just work" with zero provisioning (and sidesteps a flaky install, e.g. an MSRV gap like
/// builder3's rustc>=1.92). No paths or repo names are hardcoded: the checkout dir name is DERIVED from
/// the pinned source (git URL basename, or the crate name), and it's searched for under the data dir and
/// a few of its ancestors (so a sibling-of-the-repo or vendored-under-data-dir checkout is both found),
/// at `target/release/<bin>`. (`tools_root` is `<data-dir>/tools`.) Each hit is rev/version-verified by
/// [`local_build_matches_pin`] before use.
pub fn local_build_candidates(spec: &ToolSpec, tools_root: &Path) -> Vec<PathBuf> {
    // candidate checkout directory names, derived from the pin source (never hardcoded)
    let names: Vec<String> = match &spec.install {
        InstallSource::Git { url, .. } => vec![git_url_repo_name(url)],
        InstallSource::Registry { krate, git_fallback, .. } => {
            let mut v = vec![krate.to_string()];
            if let Some((url, _)) = git_fallback {
                let n = git_url_repo_name(url);
                if !n.is_empty() && n != *krate {
                    v.push(n);
                }
            }
            v
        }
    };
    let rel = Path::new("target").join("release").join(spec.bin_name);
    let mut out = Vec::new();
    // absolutize first: under the shipped default the data dir is RELATIVE ("gflib-data"), whose
    // .parent() chain runs out in one step — so a sibling-of-the-repo checkout would never be reached.
    let abs: PathBuf = std::path::absolute(tools_root).unwrap_or_else(|_| tools_root.to_path_buf());
    let mut root = abs.parent(); // start at <data-dir>, then walk up its ancestors
    for _ in 0..4 {
        if let Some(r) = root {
            for name in &names {
                out.push(r.join(name).join(&rel));
            }
            root = r.parent();
        }
    }
    out
}

/// Run a short, UNTRUSTED probe (a discovered binary's `--version`, a `git` query) with stdin closed
/// and a hard deadline, returning combined stdout+stderr ONLY on a clean exit — so a hanging or
/// stdin-blocked candidate can never wedge toolchain resolution. None on spawn failure, timeout, or a
/// non-zero exit. (Probe output is tiny, so reading after exit can't deadlock the pipe.)
fn probe_output(mut cmd: Command, secs: u64) -> Option<String> {
    use std::io::Read;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if t0.elapsed() > Duration::from_secs(secs) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut out);
    }
    Some(out)
}

/// Verify a local pre-built binary actually IS the pinned version, so a checkout at the wrong rev/version
/// can never be silently used. Git pin: the candidate must be its OWN repo root (guards against reporting
/// an *enclosing* repo's HEAD) AND its `HEAD` == the exact rev. Registry pin: the binary's `--version`
/// must contain the version as a whole TOKEN (not a substring — "fontc 10.6.0"/"0.6.0-pre" must NOT match
/// "0.6.0"). All probes are bounded ([`probe_output`]). Conservative — any uncertainty returns false.
pub fn local_build_matches_pin(spec: &ToolSpec, bin: &Path) -> bool {
    match &spec.install {
        InstallSource::Git { rev, .. } => {
            // bin = <checkout>/target/release/<bin> → the checkout dir is three parents up
            let dir = match bin.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
                Some(d) => d,
                None => return false,
            };
            // the candidate must be its OWN git repo root — else `rev-parse HEAD` would report an
            // enclosing repo's HEAD and could false-match the pin
            let mut top = Command::new("git");
            top.arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]);
            let toplevel = match probe_output(top, 10) {
                Some(t) => t.trim().to_string(),
                None => return false,
            };
            let is_own_root = std::fs::canonicalize(dir)
                .ok()
                .zip(std::fs::canonicalize(&toplevel).ok())
                .map(|(a, b)| a == b)
                .unwrap_or(false);
            if !is_own_root {
                return false;
            }
            let mut head = Command::new("git");
            head.arg("-C").arg(dir).args(["rev-parse", "HEAD"]);
            match probe_output(head, 10) {
                Some(h) => {
                    let h = h.trim();
                    h == rev.as_str() || h.starts_with(&spec.pin)
                }
                None => false,
            }
        }
        InstallSource::Registry { version, .. } => {
            let mut cmd = Command::new(bin);
            cmd.arg("--version");
            match probe_output(cmd, 10) {
                Some(txt) => version_token_matches(&txt, version),
                None => false,
            }
        }
    }
}

/// The toolchain signature stamped on every completed build attempt (Res.upgrade_attempted): the
/// pins + the orchestrator preference + which tools were actually AVAILABLE for the attempt.
/// Availability is part of the signature on purpose: a family built during a degraded run (e.g.
/// builder3 provisioning failed on an old rustc) re-arms for its upgrade automatically once the
/// tool becomes available — without it, the degraded stamp would suppress the upgrade forever.
pub fn run_sig(orchestrator: &str, have_fontc: bool, have_builder3: bool) -> String {
    format!(
        "builder3:{}+fontc:{}|{}|b3={}|fc={}",
        &BUILDER3_REV[..10], FONTC_VERSION, orchestrator,
        if have_builder3 { "ok" } else { "no" },
        if have_fontc { "ok" } else { "no" },
    )
}

/// `rustc --version` → e.g. (1, 91). None when rustc can't be probed (cargo install will then
/// surface its own error).
fn rustc_minor() -> Option<(u32, u32)> {
    let out = Command::new("rustc").arg("--version").output().ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    parse_rustc_minor(&txt)
}

fn parse_rustc_minor(version_line: &str) -> Option<(u32, u32)> {
    // "rustc 1.91.1 (abcdef 2026-01-01)" → (1, 91)
    let ver = version_line.split_whitespace().nth(1)?;
    let mut it = ver.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor))
}

fn meets_min_rustc(have: (u32, u32), min: &str) -> bool {
    let req = parse_rustc_minor(&format!("rustc {}", min)).unwrap_or((0, 0));
    have >= req
}

/// Run a started child to completion with a hard deadline and an optional external stop flag.
/// Polls (no extra threads); on deadline/stop the child is killed and an Err returned. The child
/// is plain `kill()` (SIGKILL): provisioning children are ours and disposable.
fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
    stop: Option<&AtomicBool>,
) -> Result<std::process::ExitStatus, String> {
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(st),
            Ok(None) => {}
            Err(e) => return Err(format!("wait: {}", e)),
        }
        if stop.map(|s| s.load(Ordering::Relaxed)).unwrap_or(false) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("interrupted by shutdown".into());
        }
        if t0.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", deadline.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Resolve one tool: explicit override → cached pin → provision → detect. Pure with respect to
/// its inputs (tools_root injectable) so tests drive it end-to-end with a fixture crate.
/// `detect` supplies the step-4 fallback (PATH/sibling probes), run only when earlier steps miss.
/// `stop` aborts an in-flight provision on daemon shutdown (verdict: Unavailable).
pub fn ensure_tool(
    spec: &ToolSpec,
    explicit: Option<&str>,
    tools_root: &Path,
    auto_provision: bool,
    stop: Option<&AtomicBool>,
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
    // 2b. an already-built, pin-matched local checkout — zero provisioning, and it survives a flaky
    // cargo install (e.g. the builder3 rustc>=1.92 MSRV gap). Verified against the pin so a wrong
    // rev/version is never silently used.
    for cand in local_build_candidates(spec, tools_root) {
        if is_executable(&cand) && local_build_matches_pin(spec, &cand) {
            return ToolStatus::Ready { path: cand.to_string_lossy().into_owned(), source: "checkout" };
        }
    }
    // 3. provision the pin
    if auto_provision {
        match provision(spec, tools_root, stop) {
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
/// <tools_root>/provision-<name>.log (truncated per session) so a failure is debuggable.
/// A per-tool lock file serializes concurrent daemons sharing one data dir; a hard deadline +
/// the stop flag bound the cargo child (killed on shutdown, never orphaned past the wait).
fn provision(spec: &ToolSpec, tools_root: &Path, stop: Option<&AtomicBool>) -> Result<PathBuf, String> {
    // MSRV preflight: fail in milliseconds with the remedy, not after minutes of compilation
    if let Some(min) = spec.min_rustc {
        if let Some(have) = rustc_minor() {
            if !meets_min_rustc(have, min) {
                return Err(format!(
                    "{}: the pinned build needs rustc >= {} but this machine has {}.{} — run `rustup update`, then restart the build to retry provisioning",
                    spec.name, min, have.0, have.1
                ));
            }
        }
    }
    let root = tools_root.join(format!("{}-{}", spec.name, spec.pin));
    let _ = std::fs::create_dir_all(tools_root);
    let log = tools_root.join(format!("provision-{}.log", spec.name));

    // ---- per-tool lock: two daemons sharing a data dir must not cargo-install into the same
    // root concurrently. Created O_EXCL; a stale lock (dead process) is broken by age. ----
    let lock = tools_root.join(format!(".provision-{}.lock", spec.name));
    let t0 = Instant::now();
    let _lockguard = loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(f) => break LockGuard { path: lock.clone(), _f: f },
            Err(_) => {
                // someone else is provisioning: maybe they finish the job for us
                let bin = provisioned_bin(tools_root, spec);
                if bin.is_file() {
                    return Ok(bin);
                }
                let stale = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|a| a > LOCK_STALE)
                    .unwrap_or(true);
                if stale {
                    let _ = std::fs::remove_file(&lock);
                    continue;
                }
                if stop.map(|s| s.load(Ordering::Relaxed)).unwrap_or(false) {
                    return Err(format!("{}: provisioning interrupted by shutdown", spec.name));
                }
                if t0.elapsed() > PROVISION_DEADLINE {
                    return Err(format!("{}: waited too long for another provisioner (lock {})", spec.name, lock.display()));
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };

    // a truncated/partial binary from an interrupted earlier install must never be mistaken
    // for a good one after a failed re-attempt
    let _ = std::fs::remove_file(provisioned_bin(tools_root, spec));

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
    for (n, args) in attempts.iter().enumerate() {
        // fresh log per session (first attempt truncates), append across this session's fallbacks
        let logf = std::fs::OpenOptions::new().create(true).write(true)
            .truncate(n == 0).append(n != 0).open(&log)
            .map_err(|e| format!("{}: open provision log: {}", spec.name, e))?;
        let logf2 = logf.try_clone().map_err(|e| format!("{}: log fd: {}", spec.name, e))?;
        let mut child = Command::new("cargo")
            .args(args)
            .arg("--root").arg(&root)
            .stdout(std::process::Stdio::from(logf))
            .stderr(std::process::Stdio::from(logf2))
            .spawn()
            .map_err(|e| format!("{}: could not run cargo (is cargo on PATH?): {}", spec.name, e))?;
        let status = match wait_with_deadline(&mut child, PROVISION_DEADLINE, stop) {
            Ok(st) => st,
            Err(e) => return Err(format!("{}: cargo install {}", spec.name, e)),
        };
        let bin = provisioned_bin(tools_root, spec);
        if status.success() && bin.is_file() {
            return Ok(bin);
        }
        // surface the CAUSE (and remedy, when known) right in the task detail — not just an rc
        let cause = std::fs::read_to_string(&log).ok().and_then(|t| summarize_cargo_log(&t));
        last = match cause {
            Some(c) => format!("{}: {} — full log: {}", spec.name, c, log.display()),
            None => format!("{}: cargo install failed (rc={}) — see {}", spec.name, status, log.display()),
        };
    }
    Err(last)
}

/// Distill a failed cargo-install log into one actionable line for the dashboard task detail /
/// control log, so the common failures explain themselves without opening the log file. Pure
/// (takes the log text) so each known pattern is unit-tested against real cargo output.
fn summarize_cargo_log(txt: &str) -> Option<String> {
    // MSRV refusal — the exact failure seen in the field:
    //   "rustc 1.91.1 is not supported by the following package:\n  ascii-dag@0.4.2 requires rustc 1.92"
    // (also covers a stale min_rustc const after a pin bump)
    if let Some(line) = txt.lines().find(|l| l.contains("is not supported by the following package")) {
        let who = txt.lines()
            .skip_while(|l| !l.contains("is not supported by the following package"))
            .nth(1)
            .map(str::trim)
            .unwrap_or("");
        return Some(format!(
            "{} ({}) — run `rustup update`, then restart the build to retry",
            line.trim().trim_end_matches(':'), who
        ));
    }
    if txt.contains("linker `cc` not found") {
        return Some("no C linker on this machine — install your distro's build-essential/gcc package, then restart the build".into());
    }
    if ["spurious network error", "Could not resolve host", "failed to fetch", "network failure", "Connection timed out"]
        .iter().any(|p| txt.contains(p))
    {
        return Some("network failure while fetching sources — check connectivity and restart the build to retry".into());
    }
    if txt.contains("no space left on device") {
        return Some("disk full — free some space, then restart the build to retry".into());
    }
    // fallback: cargo's last error line is still better than a bare exit code
    txt.lines().rev()
        .map(str::trim)
        .find(|l| l.starts_with("error") && !l.starts_with("error: failed to compile"))
        .or_else(|| txt.lines().rev().map(str::trim).find(|l| l.starts_with("error")))
        .map(str::to_string)
}

/// Removes the provision lock file on every exit path (incl. panic unwind).
struct LockGuard {
    path: PathBuf,
    _f: std::fs::File,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
/// Bounded by PROBE_DEADLINE — an arbitrary PATH binary must not be able to hang the resolver.
fn is_builder3_binary(p: &Path) -> bool {
    use std::io::Read;
    let child = Command::new(p)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return false,
    };
    if wait_with_deadline(&mut child, PROBE_DEADLINE, None).is_err() {
        return false; // hung or unkillable — not a usable builder3 either way
    }
    let mut txt = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut txt);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut txt);
    }
    let line = txt.lines().next().unwrap_or("");
    // e.g. "gftools-builder 3.0.0" — accept any 3.x; reject gftools(-builder2) 0.x
    line.contains("gftools-builder") && line.split_whitespace().any(|w| w.starts_with('3'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_token_matches_is_exact_not_substring() {
        // the real fontc line matches the pin
        assert!(version_token_matches("fontc 0.6.0", "0.6.0"));
        assert!(version_token_matches("fontc 0.6.0 (abc123 2026-01-01)", "0.6.0"));
        assert!(version_token_matches("fontc v0.6.0", "0.6.0")); // tolerate a v-prefix
        // the silent-wrong-build vectors the reviewer found must NOT match
        assert!(!version_token_matches("fontc 10.6.0", "0.6.0"));   // substring of a bigger major
        assert!(!version_token_matches("fontc 0.6.0-pre", "0.6.0")); // pre-release ≠ release
        assert!(!version_token_matches("fontc 0.6.01", "0.6.0"));
        assert!(!version_token_matches("fontc 0.16.0", "0.6.0"));
    }

    #[test]
    fn git_url_repo_name_strips_path_and_git_suffix() {
        assert_eq!(git_url_repo_name("https://github.com/simoncozens/gftools-builder3"), "gftools-builder3");
        assert_eq!(git_url_repo_name("https://github.com/googlefonts/fontc.git"), "fontc");
        assert_eq!(git_url_repo_name("git@github.com:owner/repo.git/"), "repo");
    }

    #[test]
    fn local_build_candidates_derives_names_and_searches_ancestors() {
        // tools_root = <data-dir>/tools ; a builder3 checkout is a sibling of the repo, found by walking up
        let tools = Path::new("/w/proj/gflib-build/gflib-data/tools");
        let cands = local_build_candidates(&builder3_spec(), tools);
        // name DERIVED from the git URL (not hardcoded), at target/release/<bin>, somewhere up the tree
        assert!(cands.iter().any(|p| p.ends_with("gftools-builder3/target/release/gftools-builder")),
            "expected a derived gftools-builder3 candidate, got {:?}", cands);
        assert!(cands.iter().any(|p| p == Path::new("/w/gftools-builder3/target/release/gftools-builder")),
            "expected the sibling-of-repo candidate at the workspace root, got {:?}", cands);
        // fontc (registry) derives from the crate name + the git_fallback URL
        let fc = local_build_candidates(&fontc_spec(), tools);
        assert!(fc.iter().any(|p| p.ends_with("fontc/target/release/fontc")), "got {:?}", fc);
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gflib-toolchain-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cargo_log_summaries_are_actionable() {
        // verbatim from the failed host run (2026-06-12)
        let msrv = "error: failed to compile `gftools-builder v3.0.0 (...)`\n\nCaused by:\n  rustc 1.91.1 is not supported by the following package:\n    ascii-dag@0.4.2 requires rustc 1.92\n  Either upgrade rustc or select compatible dependency versions\n";
        let s = summarize_cargo_log(msrv).unwrap();
        assert!(s.contains("rustc 1.91.1 is not supported"), "{}", s);
        assert!(s.contains("ascii-dag@0.4.2 requires rustc 1.92"), "{}", s);
        assert!(s.contains("rustup update"), "{}", s);

        let linker = "error: linker `cc` not found\n  = note: No such file or directory\n";
        assert!(summarize_cargo_log(linker).unwrap().contains("build-essential"));

        let net = "warning: spurious network error (3 tries remaining)\nerror: failed to fetch into ...\n";
        assert!(summarize_cargo_log(net).unwrap().contains("network failure"));

        let disk = "error: failed to write ...: no space left on device\n";
        assert!(summarize_cargo_log(disk).unwrap().contains("disk full"));

        // unknown failure: the last real error line, not the generic compile-failed wrapper
        let other = "   Compiling foo v1.0\nerror[E0599]: no method named `frob` found\nerror: failed to compile `bar`\n";
        assert_eq!(summarize_cargo_log(other).unwrap(), "error[E0599]: no method named `frob` found");

        // a log with no error lines yields None (caller falls back to the rc message)
        assert!(summarize_cargo_log("   Compiling foo\n    Finished\n").is_none());
    }

    #[test]
    fn rustc_msrv_parsing_and_comparison() {
        assert_eq!(parse_rustc_minor("rustc 1.91.1 (abc 2026-01-01)"), Some((1, 91)));
        assert_eq!(parse_rustc_minor("rustc 1.92.0"), Some((1, 92)));
        assert_eq!(parse_rustc_minor("garbage"), None);
        // the exact failure observed in the field: 1.91 vs builder3's lockfile needing 1.92
        assert!(!meets_min_rustc((1, 91), "1.92"));
        assert!(meets_min_rustc((1, 92), "1.92"));
        assert!(meets_min_rustc((1, 93), "1.92"));
        assert!(meets_min_rustc((2, 0), "1.92"));
        // an unparsable min never blocks (fails open to cargo's own error)
        assert!(meets_min_rustc((1, 0), "bogus"));
    }

    #[test]
    fn builder3_bin_name_contract() {
        // cargo install --root <root> lands the binary at <root>/bin/<[[bin]] name>; upstream's
        // Cargo.toml declares [[bin]] name = "gftools-builder" — BUILDER3_PKG must match it.
        let p = provisioned_bin(Path::new("/t"), &builder3_spec());
        assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("gftools-builder"));
        assert!(p.to_string_lossy().contains(&format!("builder3-{}", &BUILDER3_REV[..10])));
    }

    #[test]
    fn explicit_flag_wins_and_must_be_executable() {
        let d = tmpdir("flag");
        let spec = fontc_spec();
        // a non-executable explicit path is an error, not a silent fallback
        let f = d.join("notabin");
        std::fs::write(&f, "x").unwrap();
        let st = ensure_tool(&spec, Some(f.to_str().unwrap()), &d, false, None, || None);
        assert!(matches!(st, ToolStatus::Unavailable(_)));
        // an executable explicit path is taken verbatim, never re-provisioned
        let sh = d.join("fakebin");
        std::fs::write(&sh, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
        match ensure_tool(&spec, Some(sh.to_str().unwrap()), &d, false, None, || None) {
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
        match ensure_tool(&spec, None, &d, false, None, || Some("/decoy/builder".into())) {
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
        match ensure_tool(&spec, None, &d, false, None, || Some("/detected/fontc".into())) {
            ToolStatus::Ready { path, source } => {
                assert_eq!(source, "detected");
                assert_eq!(path, "/detected/fontc");
            }
            other => panic!("expected detected Ready, got {:?}", other),
        }
        // nothing detected either → Unavailable with an actionable message
        match ensure_tool(&spec, None, &d, false, None, || None) {
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
            min_rustc: None,
        };
        let tools = d.join("tools");
        match ensure_tool(&spec, None, &tools, true, None, || None) {
            ToolStatus::Ready { path, source } => {
                assert_eq!(source, "provisioned");
                let out = Command::new(&path).output().unwrap();
                assert!(String::from_utf8_lossy(&out.stdout).contains("gflib-fixture-tool"));
                // second resolution must hit the cache (no re-install)
                match ensure_tool(&spec, None, &tools, false, None, || None) {
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
