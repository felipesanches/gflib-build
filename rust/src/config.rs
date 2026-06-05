//! Run configuration: parsed from the CLI (a hand-rolled parser mirroring the Python argparse — no
//! external clap dependency) and persisted to `<data-dir>/gflib-build.config` as JSON so the next
//! run pre-fills the same settings. CLI flags always override the persisted file.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub source: String,        // metadata | archive
    pub data_dir: PathBuf,
    pub google_fonts: Option<PathBuf>,
    pub archive: PathBuf,
    pub archive_rev: String,
    pub build_dir: PathBuf,
    pub backend: String,       // auto | fontc | fontmake | both
    pub fontc_bin: Option<String>,
    pub builder3_bin: Option<String>,
    pub build_python: String,
    pub base_python: String,            // interpreter used to CREATE cohort venvs
    pub base_requirements: Option<PathBuf>, // pinned base toolchain (gftools/fontmake/…)
    pub build_rules: Option<PathBuf>,   // per-family pre-build commands (build_rules.json)
    pub manage_venvs: bool,
    pub jobs: usize,
    pub timeout: Option<u64>,
    pub percent: f64,
    pub only: String,
    pub retry_category: String,
    pub compare: bool,
    pub mirror_missing: bool,
    pub populate_archive: bool,
    pub retry_failed: bool,
    pub rebuild: bool,
    pub keep_work: bool,
    pub keep_fonts: bool,
    pub web_port: u16,
    pub ui: String,            // auto | curses | plain | json | none | web
    // fontspector QA pass (--fontspector): run a pinned fontspector release over all built fonts
    pub fontspector_version: String,        // the pinned release to cargo-install + record per family
    pub fontspector_profile: String,        // fontspector profile (default: googlefonts)
    pub fontspector_bin: Option<PathBuf>,   // explicit binary (else cargo-install the pinned version)
    pub fontspector_rerun: bool,            // re-QA families that already have a result (default: skip them)
    pub fontspector_qa: bool,               // SETTING: run QA asynchronously during the build (green families, niced)
    pub build_debs: bool,                   // SETTING: build+validate .deb packages during --export-deb (default off)
    pub yes: bool,
    pub dry_run: bool,         // MOCKUP: replay a previous session's outcomes (no real clone/venv/compile/QA)
    pub wizard: bool,          // force the first-run setup wizard (the editable config tab pre-build)
    pub detach: bool,          // run the build in a detached background daemon
    pub no_detach: bool,       // force foreground even for curses (which detaches by default)
    pub effreq_mirror: String, // --effreq: bare mirror to report effective requirements for
    pub effreq_commit: String, // --effreq: commit at which to read the requirements
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = PathBuf::from("gflib-data");
        Config {
            source: "metadata".into(),
            archive: data_dir.join("archive"),
            build_dir: data_dir.join("build"),
            google_fonts: None,
            data_dir,
            archive_rev: "HEAD".into(),
            backend: "auto".into(),
            fontc_bin: None,
            builder3_bin: None,
            build_python: "python3".into(),
            base_python: "python3".into(),
            base_requirements: None,
            build_rules: None,
            manage_venvs: false,
            jobs: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            timeout: None,
            percent: 100.0,
            only: String::new(),
            retry_category: String::new(),
            compare: false,
            mirror_missing: false,
            populate_archive: false,
            retry_failed: false,
            rebuild: false,
            keep_work: false,
            keep_fonts: true,
            web_port: 8765,
            ui: "auto".into(),
            fontspector_version: "1.6.0".into(),
            fontspector_profile: "googlefonts".into(),
            fontspector_bin: None,
            fontspector_rerun: false,
            fontspector_qa: false,
            build_debs: false,
            yes: false,
            dry_run: false,
            wizard: false,
            detach: false,
            no_detach: false,
            effreq_mirror: String::new(),
            effreq_commit: String::new(),
        }
    }
}

/// What the program was asked to do (decided by CLI flags), distinct from the build settings.
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Build,
    Attach,
    Stop,
    List,
    Reset,
    CohortsReport,
    EffReq,      // print the effective (post-filter/override) requirements for one mirror+commit
    Fontspector, // a separate QA pass: run fontspector over all already-built fonts
    ExportDeb,   // draft a debian/ packaging tree for every successfully-built family
    Help,
}

pub struct Parsed {
    pub cfg: Config,
    pub mode: Mode,
}

fn die(msg: &str) -> ! {
    eprintln!("{}", msg);
    std::process::exit(2);
}

/// Parse argv (already without the program name). Mirrors the Python flag surface; unknown flags
/// are reported. `--data-dir` is consulted first so build/archive defaults derive from it.
pub fn parse(args: &[String]) -> Parsed {
    let mut cfg = Config::default();
    // resolve --data-dir first so other path defaults follow it
    let mut data_dir = PathBuf::from("gflib-data");
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--data-dir" {
            if let Some(v) = args.get(i + 1) {
                data_dir = PathBuf::from(v);
            }
        }
        i += 1;
    }
    cfg.data_dir = data_dir.clone();
    cfg.archive = data_dir.join("archive");
    cfg.build_dir = data_dir.join("build");

    // load persisted config (defaults), then let CLI override below
    let cfg_path = data_dir.join("gflib-build.config");
    if let Some(loaded) = load_config(&cfg_path) {
        merge_persisted(&mut cfg, &loaded);
    }

    let mut mode = Mode::Build;
    let mut explicit_build_dir: Option<PathBuf> = None;
    let mut explicit_archive: Option<PathBuf> = None;

    let next = |i: &mut usize, name: &str| -> String {
        *i += 1;
        match args.get(*i) {
            Some(v) => v.clone(),
            None => die(&format!("{} needs a value", name)),
        }
    };

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--data-dir" => { let _ = next(&mut i, a); }
            "--source" => cfg.source = next(&mut i, a),
            "--google-fonts" => cfg.google_fonts = Some(PathBuf::from(next(&mut i, a))),
            "--archive" => explicit_archive = Some(PathBuf::from(next(&mut i, a))),
            "--archive-rev" => cfg.archive_rev = next(&mut i, a),
            "--build-dir" => explicit_build_dir = Some(PathBuf::from(next(&mut i, a))),
            "--backend" => cfg.backend = next(&mut i, a),
            "--fontc-bin" => cfg.fontc_bin = Some(next(&mut i, a)),
            "--builder3-bin" => cfg.builder3_bin = Some(next(&mut i, a)),
            "--build-python" => cfg.build_python = next(&mut i, a),
            "--base-python" => cfg.base_python = next(&mut i, a),
            "--base-requirements" => cfg.base_requirements = Some(PathBuf::from(next(&mut i, a))),
            "--build-rules" => cfg.build_rules = Some(PathBuf::from(next(&mut i, a))),
            "--manage-venvs" => cfg.manage_venvs = true,
            "--no-manage-venvs" => cfg.manage_venvs = false,
            // jobs 0 = load + inspect the latest data with NO build workers (no font building)
            "--jobs" => cfg.jobs = next(&mut i, a).parse().unwrap_or(cfg.jobs),
            "--timeout" => cfg.timeout = next(&mut i, a).parse().ok(),
            "--percent" => cfg.percent = next(&mut i, a).parse::<f64>().unwrap_or(100.0).clamp(0.0, 100.0),
            "--only" => cfg.only = next(&mut i, a),
            "--retry-category" => cfg.retry_category = next(&mut i, a),
            "--compare" => cfg.compare = true,
            "--no-compare" => cfg.compare = false,
            "--mirror-missing" => cfg.mirror_missing = true,
            "--populate-archive" => cfg.populate_archive = true,
            "--no-populate-archive" => cfg.populate_archive = false,
            "--retry-failed" => cfg.retry_failed = true,
            "--rebuild" => cfg.rebuild = true,
            "--keep-work" => cfg.keep_work = true,
            "--keep-fonts" => cfg.keep_fonts = true,
            "--discard-fonts" => cfg.keep_fonts = false,
            "--web-port" => cfg.web_port = next(&mut i, a).parse().unwrap_or(8765),
            "--ui" => cfg.ui = next(&mut i, a),
            "--yes" | "-y" => cfg.yes = true,
            "--dry-run" | "--demo" | "--mock" => cfg.dry_run = true,
            "--setup" | "--wizard" => cfg.wizard = true,
            "--detach" => cfg.detach = true,
            "--no-detach" => cfg.no_detach = true,
            "--list" => mode = Mode::List,
            // --fontspector ENABLES async QA during the build (the setting); --fontspector-pass is the
            // one-shot standalone QA of already-built fonts (no build).
            "--fontspector" => cfg.fontspector_qa = true,
            "--fontspector-pass" => mode = Mode::Fontspector,
            "--no-fontspector" => cfg.fontspector_qa = false,
            "--build-debs" => cfg.build_debs = true,
            "--no-build-debs" => cfg.build_debs = false,
            "--fontspector-version" => cfg.fontspector_version = next(&mut i, a),
            "--fontspector-profile" => cfg.fontspector_profile = next(&mut i, a),
            "--fontspector-bin" => cfg.fontspector_bin = Some(PathBuf::from(next(&mut i, a))),
            "--fontspector-rerun" => cfg.fontspector_rerun = true,
            "--cohorts-report" => mode = Mode::CohortsReport,
            "--export-deb" => mode = Mode::ExportDeb,
            "--effreq" => {
                mode = Mode::EffReq;
                cfg.effreq_mirror = next(&mut i, a);
                cfg.effreq_commit = next(&mut i, a);
            }
            "--attach" => mode = Mode::Attach,
            "--stop" => mode = Mode::Stop,
            "--reset" => mode = Mode::Reset,
            "--help" | "-h" => mode = Mode::Help,
            other => die(&format!("unknown argument: {}", other)),
        }
        i += 1;
    }

    // resolve derived path defaults honoring explicit overrides
    cfg.build_dir = explicit_build_dir.unwrap_or_else(|| cfg.data_dir.join("build"));
    cfg.archive = explicit_archive.unwrap_or_else(|| cfg.data_dir.join("archive"));
    if cfg.google_fonts.is_none() && cfg.source == "metadata" {
        cfg.google_fonts = Some(cfg.data_dir.join("google-fonts"));
    }
    Parsed { cfg, mode }
}

fn merge_persisted(cfg: &mut Config, loaded: &BTreeMap<String, serde_json::Value>) {
    use serde_json::Value;
    let s = |v: &Value| v.as_str().map(|x| x.to_string());
    for (k, v) in loaded {
        match k.as_str() {
            "source" => if let Some(x) = s(v) { cfg.source = x },
            "backend" => if let Some(x) = s(v) { cfg.backend = x },
            "fontc_bin" => cfg.fontc_bin = s(v),
            "builder3_bin" => cfg.builder3_bin = s(v),
            "build_python" => if let Some(x) = s(v) { cfg.build_python = x },
            "jobs" => if let Some(x) = v.as_u64() { if x > 0 { cfg.jobs = x as usize } }, // ignore a stale persisted 0
            "percent" => if let Some(x) = v.as_f64() { cfg.percent = x },
            "compare" => if let Some(x) = v.as_bool() { cfg.compare = x },
            "manage_venvs" => if let Some(x) = v.as_bool() { cfg.manage_venvs = x },
            "fontspector_qa" => if let Some(x) = v.as_bool() { cfg.fontspector_qa = x },
            "build_debs" => if let Some(x) = v.as_bool() { cfg.build_debs = x },
            // NOTE: 'ui' is deliberately NOT loaded — it's a per-invocation choice, not a saved
            // preference. (A prior `--ui none` must never silence a later interactive run.)
            "web_port" => if let Some(x) = v.as_u64() { cfg.web_port = x as u16 },
            _ => {}
        }
    }
}

pub fn load_config(path: &std::path::Path) -> Option<BTreeMap<String, serde_json::Value>> {
    let txt = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&txt).ok()
}

/// Persist the chosen settings (a curated subset) so the next run pre-fills them.
pub fn save_config(cfg: &Config) {
    let path = cfg.data_dir.join("gflib-build.config");
    let mut m = BTreeMap::<String, serde_json::Value>::new();
    use serde_json::json;
    m.insert("source".into(), json!(cfg.source));
    m.insert("backend".into(), json!(cfg.backend));
    m.insert("fontc_bin".into(), json!(cfg.fontc_bin));
    m.insert("builder3_bin".into(), json!(cfg.builder3_bin));
    m.insert("build_python".into(), json!(cfg.build_python));
    m.insert("jobs".into(), json!(cfg.jobs.max(1))); // never persist a transient --jobs 0 (inspect-only)
    m.insert("percent".into(), json!(cfg.percent));
    m.insert("compare".into(), json!(cfg.compare));
    m.insert("manage_venvs".into(), json!(cfg.manage_venvs));
    m.insert("fontspector_qa".into(), json!(cfg.fontspector_qa));
    m.insert("build_debs".into(), json!(cfg.build_debs));
    // 'ui' is intentionally NOT persisted (per-invocation choice — see merge_persisted).
    m.insert("web_port".into(), json!(cfg.web_port));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(txt) = serde_json::to_string_pretty(&m) {
        let _ = std::fs::write(&path, txt);
    }
}

/// Show a path relative to the cwd when it lives under it (else absolute) — mirrors the TUI's
/// `display_path`, so both UIs render the same short paths in the config tab.
fn disp_path(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    match std::path::Path::new(p).strip_prefix(&cwd) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => p.to_string(),
    }
}

/// The full config map for the snapshot — one entry per CONFIG_SCHEMA field, so the config tab (and
/// the setup wizard, which reuses this) can show and edit every setting.
pub fn config_map(cfg: &Config) -> BTreeMap<String, serde_json::Value> {
    use serde_json::json;
    let mut m = BTreeMap::new();
    m.insert("source".into(), json!(cfg.source));
    // path fields are shown relative to the cwd when under it (matches the TUI's display_path), so the
    // terminal and the browser render the same short paths
    m.insert("google_fonts".into(), json!(cfg.google_fonts.as_ref().map(|p| disp_path(&p.to_string_lossy())).unwrap_or_default()));
    m.insert("archive".into(), json!(disp_path(&cfg.archive.to_string_lossy())));
    m.insert("build_dir".into(), json!(disp_path(&cfg.build_dir.to_string_lossy())));
    m.insert("backend".into(), json!(cfg.backend));
    m.insert("fontc_bin".into(), json!(cfg.fontc_bin.as_ref().map(|p| disp_path(p)).unwrap_or_default()));
    m.insert("build_fontc".into(), json!(false));
    m.insert("jobs".into(), json!(cfg.jobs));
    m.insert("percent".into(), json!(cfg.percent));
    // store as null when unset, matching the editor's "0 → no timeout" convention (so the config tab
    // doesn't flag an unset timeout as *changed)
    m.insert("timeout".into(), cfg.timeout.map(|t| json!(t)).unwrap_or(serde_json::Value::Null));
    m.insert("populate_archive".into(), json!(cfg.populate_archive));
    m.insert("manage_venvs".into(), json!(cfg.manage_venvs));
    m.insert("retry_failed".into(), json!(cfg.retry_failed));
    m.insert("compare".into(), json!(cfg.compare));
    m.insert("fontspector_qa".into(), json!(cfg.fontspector_qa));
    m.insert("build_debs".into(), json!(cfg.build_debs));
    m
}

/// Apply the typed config the setup wizard returned (▶ Start build) back onto the Config, mirroring
/// how the Python tool maps the edited fields onto `args` before launching the build.
pub fn apply_setup_map(cfg: &mut Config, m: &BTreeMap<String, serde_json::Value>) {
    use serde_json::Value;
    let s = |k: &str| -> Option<String> { m.get(k).and_then(|v| v.as_str()).map(|s| s.to_string()) };
    if let Some(v) = s("source") { cfg.source = v; }
    cfg.google_fonts = s("google_fonts").filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(v) = s("archive").filter(|v| !v.is_empty()) { cfg.archive = PathBuf::from(v); }
    if let Some(v) = s("build_dir").filter(|v| !v.is_empty()) { cfg.build_dir = PathBuf::from(v); }
    if let Some(v) = s("backend") { cfg.backend = v; }
    cfg.fontc_bin = s("fontc_bin").filter(|v| !v.is_empty());
    if let Some(j) = m.get("jobs").and_then(|v| v.as_i64()) { cfg.jobs = j.max(0) as usize; } // 0 = inspect-only
    if let Some(p) = m.get("percent").and_then(|v| v.as_f64()) { cfg.percent = p; }
    cfg.timeout = match m.get("timeout") {
        Some(Value::Null) | None => None,
        Some(v) => v.as_u64().filter(|&t| t > 0),
    };
    if let Some(b) = m.get("populate_archive").and_then(|v| v.as_bool()) { cfg.populate_archive = b; }
    if let Some(b) = m.get("manage_venvs").and_then(|v| v.as_bool()) { cfg.manage_venvs = b; }
    if let Some(b) = m.get("retry_failed").and_then(|v| v.as_bool()) { cfg.retry_failed = b; }
    if let Some(b) = m.get("fontspector_qa").and_then(|v| v.as_bool()) { cfg.fontspector_qa = b; }
    if let Some(b) = m.get("build_debs").and_then(|v| v.as_bool()) { cfg.build_debs = b; }
    cfg.compare = m.get("compare").and_then(|v| v.as_bool()).unwrap_or(false) && cfg.source == "metadata";
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_core_flags() {
        let args: Vec<String> = ["--source","archive","--jobs","12","--percent","5","--backend","fontc","--only","a/b,c/d"]
            .iter().map(|s| s.to_string()).collect();
        let p = parse(&args);
        assert_eq!(p.cfg.source, "archive");
        assert_eq!(p.cfg.jobs, 12);
        assert_eq!(p.cfg.percent, 5.0);
        assert_eq!(p.cfg.backend, "fontc");
        assert_eq!(p.cfg.only, "a/b,c/d");
        assert_eq!(p.mode, Mode::Build);
    }
    #[test]
    fn percent_clamps() {
        let args: Vec<String> = ["--percent","250"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse(&args).cfg.percent, 100.0);
    }
    #[test]
    fn lifecycle_modes() {
        assert_eq!(parse(&["--list".into()]).mode, Mode::List);
        assert_eq!(parse(&["--stop".into()]).mode, Mode::Stop);
        assert_eq!(parse(&["--reset".into()]).mode, Mode::Reset);
    }
    #[test]
    fn data_dir_drives_path_defaults() {
        let args: Vec<String> = ["--data-dir","/tmp/xyz"].iter().map(|s| s.to_string()).collect();
        let p = parse(&args);
        assert_eq!(p.cfg.build_dir, std::path::PathBuf::from("/tmp/xyz/build"));
        assert_eq!(p.cfg.archive, std::path::PathBuf::from("/tmp/xyz/archive"));
    }
}
