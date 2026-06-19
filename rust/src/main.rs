//! gflib-build (Rust port) — entry point and run-mode dispatch.
//!
//! A from-scratch, archive-safe harness to build the entire Google Fonts library locally, with a
//! live TUI, a web dashboard, a parallel build engine, and resumable state — kept schema-compatible
//! with the original Python implementation's schema (same status.json / control.json / state.json), so the two
//! interoperate: you can watch a Python daemon from this Rust TUI and vice-versa.

mod build;
mod classify;
mod config;
mod crater;
mod daemon;
mod deb;
mod discover;
mod fontspector;
mod mirror;
mod model;
mod monitor;
mod persist;
mod provenance;
mod rules;
mod toolchain;
mod tui;
mod util;
mod venv;
mod web;

use config::Mode;
use monitor::{MonitorState, Source};
use std::io::IsTerminal;
use std::sync::Arc;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = config::parse(&argv);
    let cfg = parsed.cfg;

    match parsed.mode {
        Mode::Help => print_help(),
        Mode::List => run_list(&cfg),
        Mode::Stop => run_stop(&cfg),
        Mode::Reset => run_reset(&cfg),
        Mode::CohortsReport => run_cohorts_report(&cfg),
        Mode::EffReq => run_effreq(&cfg),
        Mode::ExportDeb => deb::run_export_deb(&cfg),
        Mode::Fontspector => std::process::exit(fontspector::run_pass(&cfg)),
        Mode::Attach => run_attach(&cfg),
        Mode::Build => run_build(cfg),
    }
}

fn pick_frontend(ui: &str) -> String {
    match ui {
        "auto" => {
            if std::io::stdout().is_terminal() {
                "curses".into()
            } else {
                "plain".into()
            }
        }
        other => other.into(),
    }
}

/// --dry-run / --demo: a fully in-process MOCKUP for demos. Loads a previous session's data and
/// replays the build (looping) with NO real clone/venv/compile/QA and NO writes to disk — so the
/// dashboard looks live without any CPU load (keeps a video call's audio/video smooth). View it with
/// `--ui curses` (default) or `--ui web`; press q to quit.
fn run_dry_run(cfg: config::Config) {
    eprintln!(
        "gflib-build DRY RUN (mockup): replaying {} with no real compilation — nothing is written to disk.",
        cfg.build_dir.display()
    );
    let orch = build::Orchestrator::new(cfg.clone());
    orch.start();
    let source: Arc<dyn Source> = orch.clone();
    match pick_frontend(&cfg.ui).as_str() {
        "web" => {
            let _ = web::run(source, cfg.web_port);
        }
        "curses" => {
            if std::io::stdout().is_terminal() {
                let _ = tui::run(source);
            } else {
                run_plain(&source);
            }
        }
        "json" => run_json(&source),
        "none" => run_none(&source),
        _ => run_plain(&source),
    }
    orch.request_stop();
}

fn run_build(mut cfg: config::Config) {
    // fontc/builder3 resolution happens inside the Orchestrator (toolchain.rs): explicit flag →
    // provisioned pin → auto-provision → detected. cfg.fontc_bin stays the USER's override only.
    if !cfg.archive.is_dir() {
        if let Some(a) = discover::detect_archive(&cfg.data_dir) {
            cfg.archive = std::path::PathBuf::from(a);
        }
    }
    // auto-detect build_rules.json (version-controlled next to the tool / in CWD)
    if cfg.build_rules.is_none() {
        for c in ["build_rules.json", "../build_rules.json"] {
            let p = std::path::PathBuf::from(c);
            if p.is_file() {
                cfg.build_rules = Some(p);
                break;
            }
        }
    }
    // auto-detect the base toolchain requirements (so --manage-venvs reuses the same venvs the
    // Python tool built — the cohort marker hashes the base lines, so the file content must match)
    if cfg.manage_venvs && cfg.base_requirements.is_none() {
        for c in ["requirements-build.txt", "../requirements-build.txt"] {
            let p = std::path::PathBuf::from(c);
            if p.is_file() {
                cfg.base_requirements = Some(p);
                break;
            }
        }
    }
    // --dry-run MOCKUP: replay the saved session in-process (no daemon, no persistence, no real work)
    if cfg.dry_run {
        return run_dry_run(cfg);
    }
    // if a daemon is already running here, just attach a monitor (don't start a second build)
    if let Some(pid) = persist::read_daemon_pid(&cfg.build_dir) {
        eprintln!(
            "a build is already running at {} (pid {}) — attaching a live monitor.",
            cfg.build_dir.display(),
            pid
        );
        return run_attach(&cfg);
    }
    let ui = pick_frontend(&cfg.ui);

    // ---- first-run setup wizard: the editable config tab, pre-build (launch on ▶ Start build).
    //      Triggered by --setup/--wizard or a missing google/fonts clone (metadata mode), when
    //      interactive and not --yes — mirroring the Python first-run flow. ----
    let need_gf = cfg.source == "metadata"
        && !cfg.google_fonts.as_ref().map(|p| p.join("ofl").is_dir()).unwrap_or(false);
    if (cfg.wizard || need_gf) && !cfg.yes && ui == "curses" && std::io::stdin().is_terminal() {
        let setup_src: Arc<dyn Source> =
            monitor::SetupState::new(config::config_map(&cfg), cfg.build_dir.clone());
        match tui::run_mode(setup_src, true) {
            Ok(tui::TuiResult::StartBuild(m)) => config::apply_setup_map(&mut cfg, &m),
            _ => {
                eprintln!("aborted.");
                return;
            }
        }
    }

    config::save_config(&cfg);

    // Detach by default for the interactive curses UI (quit the monitor, build keeps running);
    // --detach forces it for any UI; --no-detach keeps curses in the foreground. daemonize() MUST
    // run before Orchestrator::new (whose discovery spawns threads) so the daemon keeps the pool.
    let want_detach = cfg.detach || (ui == "curses" && !cfg.no_detach);
    if want_detach {
        if daemon::daemonize(&cfg.build_dir) {
            // DAEMON: run the build headless and linger ~30 min after completion (so a live retry works)
            let orch = build::Orchestrator::new(cfg.clone());
            orch.start();
            daemon::run_daemon(&orch, std::time::Duration::from_secs(30 * 60));
            return;
        }
        // PARENT: wait briefly for the daemon to write its pid, then attach a live monitor
        eprintln!("gflib-build (Rust): build detached at {} — attaching a live monitor (q leaves it running; --stop to cancel).", cfg.build_dir.display());
        for _ in 0..50 {
            if persist::read_daemon_pid(&cfg.build_dir).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        return run_attach(&cfg);
    }

    // foreground (plain/json/none/web, or curses with --no-detach)
    eprintln!(
        "gflib-build (Rust): source={} backend={} jobs={} build_dir={}",
        cfg.source, cfg.backend, cfg.jobs, cfg.build_dir.display()
    );
    let orch = build::Orchestrator::new(cfg.clone());
    persist::write_pid(&cfg.build_dir);
    daemon::install_sigterm_handler();
    orch.start();
    let source: Arc<dyn Source> = orch.clone();

    match ui.as_str() {
        "web" => {
            let _ = web::run(source, cfg.web_port);
        }
        "curses" => {
            let _ = tui::run(source);
        }
        "none" => run_none(&source),
        "json" => run_json(&source),
        _ => run_plain(&source),
    }

    orch.finalize(); // synchronous final status + migration.json + timings.json before we exit
    orch.request_stop();
    persist::clear_pid(&cfg.build_dir);
    daemon::respawn_if_requested(&orch); // UI "Restart": re-launch after a clean foreground shutdown
}

fn run_attach(cfg: &config::Config) {
    let mon = MonitorState::new(&cfg.build_dir);
    if !mon.daemon_alive() {
        eprintln!(
            "note: no live daemon at {} (showing last status.json if any)",
            cfg.build_dir.display()
        );
    }
    let source: Arc<dyn Source> = mon;
    let ui = pick_frontend(&cfg.ui);
    match ui.as_str() {
        "web" => {
            let _ = web::run(source, cfg.web_port);
        }
        "curses" => {
            if !std::io::stdout().is_terminal() {
                eprintln!("--attach with curses needs a terminal; falling back to plain");
                run_plain(&source);
            } else {
                let _ = tui::run(source);
            }
        }
        "none" => run_none(&source),
        "json" => run_json(&source),
        _ => run_plain(&source),
    }
}

fn run_stop(cfg: &config::Config) {
    match persist::read_daemon_pid(&cfg.build_dir) {
        Some(pid) => {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            unsafe {
                kill(pid, 15); // SIGTERM
            }
            eprintln!("sent stop to build daemon {} at {}", pid, cfg.build_dir.display());
        }
        None => eprintln!("no running build daemon at {}", cfg.build_dir.display()),
    }
}

fn run_list(cfg: &config::Config) {
    let mut cfg = cfg.clone();
    if !cfg.archive.is_dir() {
        if let Some(a) = discover::detect_archive(&cfg.data_dir) {
            cfg.archive = std::path::PathBuf::from(a);
        }
    }
    let (fams, total, skipped) = match cfg.source.as_str() {
        "archive" => discover::discover_archive(&cfg.archive, &cfg.archive_rev, cfg.jobs, None),
        _ => match &cfg.google_fonts {
            Some(gf) => discover::discover_metadata(gf),
            None => {
                eprintln!("--source metadata needs a google/fonts clone (--google-fonts)");
                return;
            }
        },
    };
    for f in &fams {
        println!("{}\t{}\t{}", f.slug, f.commit, f.url);
    }
    eprintln!(
        "{} buildable / {} in library ({} skipped: no config or no pinned commit)",
        fams.len(),
        total,
        skipped
    );
}

/// `--effreq <mirror.git> <commit>`: print the cohort key + the EXACT effective requirements the
/// installer would feed pip for that repo at that commit — include-expanded, QA-filtered, with family
/// pins overriding the base toolchain and pin-overrides applied. A read-only diagnostic (one
/// `git show` per requirements file; no extraction, no install). Useful for debugging venv failures.
fn run_effreq(cfg: &config::Config) {
    let mirror = std::path::PathBuf::from(&cfg.effreq_mirror);
    if !mirror.is_dir() {
        eprintln!("--effreq: mirror not found: {}", mirror.display());
        std::process::exit(2);
    }
    // base toolchain: explicit --base-requirements, else the bundled requirements-build.txt
    let base_path = cfg.base_requirements.clone().or_else(|| {
        ["requirements-build.txt", "../requirements-build.txt"]
            .iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.is_file())
    });
    let base_text = base_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();

    let req = venv::read_requirements_from_mirror(&mirror, &cfg.effreq_commit);
    let key = venv::cohort_key_for(&req);
    let base_lines = venv::filter_qa_requirements(
        &base_text.lines().map(|s| s.to_string()).collect::<Vec<_>>(),
    );
    let requested = venv::assemble_requested(&base_lines, &req, &key);
    let (effective, overridden) = venv::apply_pin_overrides(&requested);

    println!("# mirror: {}", mirror.display());
    println!("# commit: {}", cfg.effreq_commit);
    println!(
        "# base:   {}",
        base_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into())
    );
    println!("# cohort-key: {}", key);
    if !overridden.is_empty() {
        println!("# pin-overrides applied: {}", overridden.join(", "));
    }
    println!("# --- effective requirements (what pip resolves) ---");
    for l in &effective {
        println!("{}", l);
    }
}

fn run_cohorts_report(cfg: &config::Config) {
    // Read-only preview of the dependency-cohort grouping: scan each family's requirements via
    // `git show` on the mirror (no extraction, no builds, archives untouched). Ported from Python.
    let mut cfg = cfg.clone();
    if !cfg.archive.is_dir() {
        if let Some(a) = discover::detect_archive(&cfg.data_dir) {
            cfg.archive = std::path::PathBuf::from(a);
        }
    }
    let (fams, _total, _skipped) = match cfg.source.as_str() {
        "archive" => discover::discover_archive(&cfg.archive, &cfg.archive_rev, cfg.jobs, None),
        _ => match &cfg.google_fonts {
            Some(gf) => discover::discover_metadata(gf),
            None => {
                eprintln!("--cohorts-report with --source metadata needs --google-fonts");
                return;
            }
        },
    };
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut sigs: BTreeMap<String, String> = BTreeMap::new();
    for f in &fams {
        let mp = build::mirror_path(&cfg.archive, &f.url);
        let (cohort, sig) = if !mp.is_dir() {
            ("(mirror-absent)".to_string(), String::new())
        } else {
            let req = venv::read_requirements_from_mirror(&mp, &f.commit);
            (venv::cohort_key_for(&req), venv::normalize_requirements(&req))
        };
        groups.entry(cohort.clone()).or_default().push(f.slug.clone());
        sigs.entry(cohort).or_insert(sig);
    }
    let real = groups.keys().filter(|k| *k != "base" && *k != "(mirror-absent)").count();
    println!(
        "Cohort report: {} repos scanned -> {} distinct dependency cohort(s), plus 'base' and any mirror-absent.\n",
        fams.len(), real
    );
    let mut ordered: Vec<_> = groups.iter().collect();
    ordered.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    for (cohort, slugs) in &ordered {
        let label = match cohort.as_str() {
            "base" => "base — no requirements file".to_string(),
            "(mirror-absent)" => "mirror absent — not scanned".to_string(),
            other => other.to_string(),
        };
        println!("== {}  ·  {} families ==", label, slugs.len());
        if let Some(sig) = sigs.get(*cohort) {
            for line in sig.lines().take(6) {
                println!("    {}", line);
            }
        }
        let shown: Vec<&str> = slugs.iter().take(8).map(|s| s.as_str()).collect();
        println!("    {}{}\n", shown.join(", "), if slugs.len() > 8 { ", …" } else { "" });
    }
    // write cohorts.json next to the build dir
    let _ = std::fs::create_dir_all(&cfg.build_dir);
    let out: BTreeMap<&String, &Vec<String>> = groups.iter().collect();
    if let Ok(txt) = serde_json::to_string_pretty(&out) {
        let p = cfg.build_dir.join("cohorts.json");
        if std::fs::write(&p, txt).is_ok() {
            eprintln!("wrote {}", p.display());
        }
    }
}

fn run_reset(cfg: &config::Config) {
    // delete the whole build dir; NEVER touch the archive (append-only policy)
    let bd = &cfg.build_dir;
    if let Ok(ar) = cfg.archive.canonicalize() {
        if let Ok(bdc) = bd.canonicalize() {
            if ar == bdc || ar.starts_with(&bdc) {
                eprintln!("refusing to reset: the archive lives inside the build dir — move it first");
                return;
            }
        }
    }
    if persist::read_daemon_pid(bd).is_some() {
        eprintln!("refusing to reset: a build is running here (--stop first)");
        return;
    }
    if !cfg.yes {
        eprintln!(
            "would delete {} (all built assets + venvs). Re-run with --yes to confirm.",
            bd.display()
        );
        return;
    }
    match std::fs::remove_dir_all(bd) {
        Ok(_) => eprintln!("reset: deleted {} (archive untouched)", bd.display()),
        Err(e) => eprintln!("reset failed: {}", e),
    }
}

// ---- non-interactive frontends ----

fn run_plain(source: &Arc<dyn Source>) {
    loop {
        let snap = source.snapshot();
        let c = &snap.counts;
        eprintln!(
            "[{}] built {} failed {} building {} queued {} | {} | disk {}",
            util::hms(snap.elapsed),
            c.built,
            c.failed,
            c.building,
            c.queued,
            snap.phase,
            util::human(snap.disk_build_total + snap.disk_archive_total),
        );
        if (snap.done || daemon::sigterm_received()) && source.is_live() {
            eprintln!(
                "done — built {} · failed {} · skipped {}",
                c.built, c.failed, c.skipped
            );
            return;
        }
        if !source.is_live() && !snap.daemon_alive {
            eprintln!("daemon is no longer running.");
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn run_json(source: &Arc<dyn Source>) {
    loop {
        let snap = source.snapshot();
        if let Ok(s) = serde_json::to_string(&snap) {
            println!("{}", s);
        }
        if (snap.done || daemon::sigterm_received()) && source.is_live() {
            return;
        }
        if !source.is_live() && !snap.daemon_alive {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn run_none(source: &Arc<dyn Source>) {
    // silent: keep the build alive until done (status/state files still written)
    loop {
        let snap = source.snapshot();
        if (snap.done || daemon::sigterm_received()) && source.is_live() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn print_help() {
    println!(
        r#"gflib-build — build the Google Fonts library locally

USAGE:
  gflib-build [OPTIONS]

WORKLIST:
  --source <metadata|archive>   where the worklist comes from (default metadata)
  --google-fonts <DIR>          a google/fonts clone (METADATA.pb) for --source metadata
  --archive <DIR>               the repo archive of bare mirrors ({{owner}}/{{repo}}.git)
  --archive-rev <REV>           commit to build for --source archive (default HEAD)
  --data-dir <DIR>              base dir for defaults (default gflib-data)
  --build-dir <DIR>             output dir (default <data-dir>/build; never inside a repo)
  --percent <P>                 build an evenly-spaced P% sample (default 100)
  --only <a,b>                  restrict the run to an explicit comma list of slugs

BUILD:
  --backend <auto|fontc|fontmake|both>   compiler (default auto = fontc-first)
  --orchestrator <auto|builder3|builder2>  default auto: prefer gftools-builder3 (Rust), fall back
                                per family to builder2+fontc, then builder2+fontmake. builder3/
                                builder2 force that orchestrator only (builder3 = no Python fallback).
  --python-policy <off|selective|on>  Rust-only build mode (default on). off = no Python anywhere:
                                force builder3+fontc (no builder2), skip cohort venvs, refuse Python
                                pre-build rules (shell pre-build still runs). selective = off except
                                families on the per-family allow-list. Aliases: --no-python (=off),
                                --python (=on). Live-editable in the config tab.
  --fontc-bin <PATH>            explicit fontc binary (default: auto — the provisioned pin, else detected)
  --builder3-bin <PATH>         explicit gftools-builder3 binary (default: auto, like fontc)
  --no-toolchain-provision      don't cargo-install missing pinned tools (fontc/builder3); detection only.
                                Zero-setup default: the pins auto-install once into <data-dir>/tools/.
  --no-auto-upgrade             don't automatically re-attempt built families at better rungs.
                                Default ON: a family built with fontmake (or fontc under builder2)
                                is re-attempted with builder3/fontc — once per toolchain pin, queued
                                AFTER all new work. A failed upgrade keeps the existing result; a
                                successful one keeps the superseded binaries under
                                <build-dir>/variants/<family>/ for later comparison.
  --build-python <PATH>         interpreter for builds (default python3)
  --pythons <a,b,c|auto>        cohort-venv Python ladder, newest→oldest (e.g. python3.13,python3.11), or
                                'auto' to discover installed python3.N. A cohort whose exact pinned reqs
                                have no wheel on a rung falls back to an older one (keeping the pins)
                                before relaxing; the commit year picks the starting rung. Single = legacy.
  --jobs <N>                    parallel workers (default = CPU count). Each build is confined to a
                                budget of ~cpus/jobs CPUs (taskset slice + RAYON_NUM_THREADS +
                                builder3 --jobs), so heavily-parallel children (ninja, fontc,
                                builder3, pip sdist builds) cannot multiply into cpus² load.
                                Lower --jobs = fewer, fatter slices. --no-cpu-slices disables the
                                taskset confinement (the soft caps remain).
  --timeout <SECS>              per-build timeout (default: none)
  --compare                     sha256-compare built fonts to shipped (metadata mode)
  --retry-failed / --rebuild    re-attempt failures / ignore prior state
  --retry-category <CAUSE>      re-attempt only failures with this cause (see the failures tab)
  --retrigger <a,b>             force-rebuild an explicit slug list regardless of prior status
                                (the "I just applied a fix, rebuild the affected families" path)

FONTC_CRATER COMPARISON (compare our build status to fontc_crater's latest run):
  --crater <PATH>               fontc_crater status file (default: auto-resolve
                                fontc_crater_targets.json, then the diff-only analysis file, in
                                gflib-data and the sibling gfonts_agents/data)
  --no-crater                   disable the fontc_crater comparison
  --retrigger-crater <MODE>     force-rebuild families by crater verdict, where MODE is
                                fontc-failed | both-failed | failed | diff. e.g.
                                --retrigger-crater fontc-failed rebuilds every family
                                fontc_crater's fontc cannot compile — to find the ones WE can
                                (their config.yaml / build rules then unblock crater too).

UI:
  --ui <auto|curses|plain|json|none|web>   frontend (default auto)
  --web-port <PORT>             port for --ui web (default 8765)

LIFECYCLE:
  --dry-run / --demo / --mock   MOCKUP: replay a previous session's data live (no real clone/venv/
                                compile/QA, no disk writes) — a CPU-light demo. View with --ui curses/web.
  --setup / --wizard            open the editable Configuration tab pre-build; launch on ▶ Start build
  --fontspector                 enable async fontspector QA during the build: a pinned release runs
                                (niced) on each green-built family, results in the 'fontspector' tab.
                                Also a config-tab/wizard setting (fontspector_qa). --no-fontspector off.
  --fontspector-pass            one-shot: QA all already-built fonts now and exit (no build).
                                shared opts: --fontspector-version <v> / -profile <p> / -bin <path> / -rerun
  --list                        print the buildable worklist and exit
  --cohorts-report              preview the dependency-cohort grouping (read-only) and exit
  --effreq <mirror.git> <commit>  print the effective requirements a repo would feed pip (read-only)
  --attach                      attach a read-only monitor to a build at --build-dir
  --detach / --no-detach        run the build in a background daemon (default for curses) / force fg
  --stop                        signal a build daemon at --build-dir to stop (graceful)
  --reset --yes                 delete the whole build dir (archive is NEVER touched)
"#
    );
}
