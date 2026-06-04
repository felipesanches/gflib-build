//! gflib-build (Rust port) — entry point and run-mode dispatch.
//!
//! A from-scratch, archive-safe harness to build the entire Google Fonts library locally, with a
//! live TUI, a web dashboard, a parallel build engine, and resumable state — kept schema-compatible
//! with the Python `gflib_build.py` (same status.json / control.json / state.json), so the two
//! interoperate: you can watch a Python daemon from this Rust TUI and vice-versa.

mod build;
mod config;
mod discover;
mod model;
mod monitor;
mod persist;
mod provenance;
mod tui;
mod util;
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

fn run_build(mut cfg: config::Config) {
    // auto-detect fontc + a pre-existing repo archive if not present
    if cfg.fontc_bin.is_none() {
        cfg.fontc_bin = discover::detect_fontc();
    }
    if !cfg.archive.is_dir() {
        if let Some(a) = discover::detect_archive(&cfg.data_dir) {
            cfg.archive = std::path::PathBuf::from(a);
        }
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
    config::save_config(&cfg);
    let ui = pick_frontend(&cfg.ui);
    eprintln!(
        "gflib-build (Rust): source={} backend={} jobs={} build_dir={}",
        cfg.source,
        cfg.backend,
        cfg.jobs,
        cfg.build_dir.display()
    );

    let orch = build::Orchestrator::new(cfg.clone());
    persist::write_pid(&cfg.build_dir);
    orch.start();
    let source: Arc<dyn Source> = orch.clone();

    match ui.as_str() {
        "web" => {
            let _ = web::run(source, cfg.web_port);
        }
        "curses" => {
            if let Ok(tui::TuiResult::Reconfigure) = tui::run(source) {
                eprintln!("(reconfigure not yet implemented in the Rust port — re-run with new flags)");
            }
        }
        "none" => run_none(&source),
        "json" => run_json(&source),
        _ => run_plain(&source),
    }

    orch.request_stop();
    persist::clear_pid(&cfg.build_dir);
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
        if snap.done && source.is_live() {
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
        if snap.done && source.is_live() {
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
        if snap.done && source.is_live() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn print_help() {
    println!(
        r#"gflib-build (Rust port) — build the Google Fonts library locally

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
  --fontc-bin <PATH>            fontc (Rust) binary (auto-detected)
  --builder3-bin <PATH>         gftools-builder3 (Rust orchestrator) — M5/M7 path
  --build-python <PATH>         interpreter for builds (default python3)
  --jobs <N>                    parallel workers (default = CPU count)
  --timeout <SECS>              per-build timeout (default: none)
  --compare                     sha256-compare built fonts to shipped (metadata mode)
  --retry-failed / --rebuild    re-attempt failures / ignore prior state

UI:
  --ui <auto|curses|plain|json|none|web>   frontend (default auto)
  --web-port <PORT>             port for --ui web (default 8765)

LIFECYCLE:
  --list                        print the buildable worklist and exit
  --attach                      attach a read-only monitor to a build at --build-dir
  --stop                        signal a build daemon at --build-dir to stop
  --reset --yes                 delete the whole build dir (archive is NEVER touched)

M0 provenance (compiler + builder, success or failure) is recorded for every attempt.
This is a Rust port; see README.md for parity with the Python tool and known gaps."#
    );
}
