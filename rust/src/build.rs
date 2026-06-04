//! The build engine (a port of the Python `Orchestrator`): a worker pool that, for each family,
//! streams its pristine source out of the bare mirror with `git archive` (read-only — archives are
//! never touched), pre-cleans committed build outputs, resolves the gftools-builder config, runs the
//! build (fontc-first, fontmake fallback, or builder3), collects the freshly-built fonts into the
//! separate build dir, and records M0 provenance (compiler + orchestrator + versions) on success AND
//! failure. State is persisted to status.json / state.json / failure-history.jsonl; a monitor drives
//! live config via control.json. UI-agnostic: it just exposes `snapshot()`.

use crate::config::{config_map, Config};
use crate::model::*;
use crate::provenance::{builder_version_str, compiler_version_str};
use crate::util::{dir_size, free_bytes, now, slug_to_logname};
use crate::venv::VenvManager;
use crate::{discover, persist, venv};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

const OUTPUT_DIRS_TO_CLEAN: [&str; 11] = [
    "fonts", "instance_ufos", "instance_ufo", "master_ufo", "master_ufos", "variable_ttf",
    "variable", "build", "out", "output", "instances",
];
const CONFIG_CANDIDATES: [&str; 4] =
    ["sources/config.yaml", "sources/config.yml", "config.yaml", "config.yml"];
const EXTRACT_TIMEOUT: u64 = 3600;
pub const MAX_JOBS: usize = 256;

/// Mutable shared state, guarded by one mutex; `snapshot()` reads it, workers mutate it.
pub struct Shared {
    pub results: BTreeMap<String, Res>,
    pub queue: VecDeque<String>,
    pub families: BTreeMap<String, Family>,
    pub phase: String,
    pub phase_label: String,
    pub paused: bool,
    pub jobs: usize,
    pub percent: f64,
    pub backend: String,
    pub compare: bool,
    pub cver_cache: HashMap<(String, String), String>,
    pub bver_cache: HashMap<(String, String), String>,
    pub failure_history: Vec<FailHist>,
    pub control_log: Vec<String>,
    pub library_total: usize,
    #[allow(dead_code)] // library families outside the worklist (reported via --list / phase_total)
    pub skipped_total: usize,
    pub disk_build_total: u64,
    pub disk_archive_total: u64,
    pub disk_archive_nested: bool,
    pub disk_free: u64,
    pub archive_total: usize,
    // cohort map (R1: preserved across resume/migration; populated by R2's VenvManager later)
    pub cohort_members: BTreeMap<String, Vec<String>>,
    pub cohort_reqs: BTreeMap<String, String>,
    pub cached_cohorts: HashSet<String>, // cohorts with a venv on disk (off-thread; for the 'cached' flag)
}

pub struct Orchestrator {
    pub cfg: Config,
    pub shared: Arc<Mutex<Shared>>,
    pub cond: Arc<Condvar>,
    pub stop: Arc<AtomicBool>,
    pub start_time: f64,
    pub resumed_elapsed: f64,
    pub active: AtomicUsize,
    pub spawned: Mutex<usize>,
    pub venvs: Option<VenvManager>, // cohort venv manager (R2); None unless --manage-venvs
    pub build_rules: std::collections::HashMap<String, Vec<String>>, // per-family pre-build (R3)
    pub all_families: Vec<Family>,  // full discovered list (R6: raising % enqueues more from here)
    pub clone_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>, // per-repo clone lock (--mirror-missing)
}

impl Orchestrator {
    /// Build the worklist (metadata or archive), reconcile with prior state.json, enqueue, and
    /// return the orchestrator ready to `start()`.
    pub fn new(cfg: Config) -> Arc<Self> {
        // --only restricts the whole run to an explicit subset (highest priority); pass it into
        // archive discovery so we don't resolve all ~1300 mirrors just to build a handful.
        let want: Option<HashSet<String>> = if cfg.only.trim().is_empty() {
            None
        } else {
            Some(
                cfg.only
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        };
        let (mut fams, library_total, skipped) = match cfg.source.as_str() {
            "archive" => discover::discover_archive(&cfg.archive, &cfg.archive_rev, cfg.jobs, want.as_ref()),
            _ => match &cfg.google_fonts {
                Some(gf) => discover::discover_metadata(gf),
                None => (Vec::new(), 0, 0),
            },
        };
        fams.sort_by(|a, b| a.slug.cmp(&b.slug));
        let all_families = fams.clone(); // full list kept so raising --percent live can enqueue more

        if let Some(w) = &want {
            fams.retain(|f| w.contains(&f.slug));
        } else {
            fams = discover::sample_evenly(fams, cfg.percent);
        }

        let state = persist::read_state_full(&cfg.build_dir);
        let prior = &state.results;
        let mut results = BTreeMap::new();
        let mut families = BTreeMap::new();
        // (slug, prior_duration) for queued families — sorted longest-first below to shrink the tail
        let mut queued_with_dur: Vec<(String, f64)> = Vec::new();
        for f in fams {
            let slug = f.slug.clone();
            families.insert(slug.clone(), f);
            let prev = prior.get(&slug);
            // resume: keep a prior success unless --rebuild; re-queue a failure if the user forces it
            // OR (self-heal, matching Python) its cause is in the AUTO_RETRY set — a fresh attempt can
            // clear a rebuilt venv / retried clone / updated mirror, so the failure hints stay honest.
            let (status, kind) = match prev {
                Some(p) if p.status == "built" && !cfg.rebuild => ("built", ""),
                Some(p) if p.status == "failed" => {
                    let (cause, _) = crate::classify::categorize_failure(&p.error);
                    let retry = cfg.rebuild
                        || cfg.retry_failed
                        || crate::classify::is_auto_retry(cause)
                        || (!cfg.retry_category.is_empty() && cause == cfg.retry_category);
                    if retry {
                        ("queued", "retry")
                    } else {
                        ("failed", "")
                    }
                }
                _ => ("queued", "new"),
            };
            let prior_dur = prev
                .map(|p| (p.ended - p.started).max(0.0))
                .filter(|d| *d > 0.0)
                .unwrap_or(0.0);
            let mut r = prev.cloned().unwrap_or_else(|| Res {
                slug: slug.clone(),
                ..Default::default()
            });
            r.slug = slug.clone();
            r.status = status.into();
            if status == "queued" {
                r.queued_kind = if cfg.rebuild { "rebuild".into() } else { kind.into() };
                queued_with_dur.push((slug.clone(), prior_dur));
            }
            results.insert(slug, r);
        }
        // longest-first: families with a known long prior build go first; never-built (dur 0) last
        queued_with_dur.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let queue: VecDeque<String> = queued_with_dur.into_iter().map(|(s, _)| s).collect();

        let failure_history = persist::read_failure_history(&cfg.build_dir);
        let jobs = cfg.jobs.clamp(1, MAX_JOBS);
        let shared = Shared {
            results,
            queue,
            families,
            phase: "build".into(),
            phase_label: String::new(),
            paused: false,
            jobs,
            percent: cfg.percent,
            backend: cfg.backend.clone(),
            compare: cfg.compare,
            cver_cache: HashMap::new(),
            bver_cache: HashMap::new(),
            failure_history,
            control_log: Vec::new(),
            library_total,
            skipped_total: skipped,
            disk_build_total: 0,
            disk_archive_total: 0,
            disk_archive_nested: false,
            disk_free: 0,
            archive_total: 0,
            cohort_members: state.cohort_members,
            cohort_reqs: state.cohort_reqs,
            cached_cohorts: HashSet::new(),
        };
        let venvs = if cfg.manage_venvs {
            Some(VenvManager::new(&cfg.build_dir, &cfg.base_python, cfg.base_requirements.clone()))
        } else {
            None
        };
        let build_rules = cfg
            .build_rules
            .as_ref()
            .map(|p| crate::rules::load_build_rules(p))
            .unwrap_or_default();
        Arc::new(Orchestrator {
            cfg,
            shared: Arc::new(Mutex::new(shared)),
            cond: Arc::new(Condvar::new()),
            stop: Arc::new(AtomicBool::new(false)),
            start_time: now(),
            resumed_elapsed: state.elapsed_so_far, // cumulative clock survives restart/migration (R1)
            active: AtomicUsize::new(0),
            spawned: Mutex::new(0),
            venvs,
            build_rules,
            all_families,
            clone_locks: Mutex::new(HashMap::new()),
        })
    }

    /// Spawn the worker pool, the status writer, the disk-size thread, and the control watcher.
    pub fn start(self: &Arc<Self>) {
        let _ = std::fs::create_dir_all(&self.cfg.build_dir);
        let _ = std::fs::create_dir_all(self.cfg.build_dir.join("logs"));
        // Build the base cohort venv up front (on a background thread so it doesn't block startup) —
        // avoids a stampede where every base-cohort worker tries to create it at once. Workers that
        // reach a base-cohort family before it's ready just wait on the per-cohort lock (correct).
        if self.venvs.is_some() {
            let me = Arc::clone(self);
            thread::spawn(move || {
                if let Some(v) = &me.venvs {
                    if let Err(e) = v.ensure_base() {
                        let mut sh = me.shared.lock().unwrap();
                        sh.control_log.push(format!("base venv: {}", e));
                    }
                }
            });
        }
        let jobs = self.shared.lock().unwrap().jobs;
        self.ensure_workers(jobs);
        self.spawn_status_writer();
        self.spawn_size_thread();
        self.spawn_control_watcher();
    }

    fn ensure_workers(self: &Arc<Self>, n: usize) {
        let mut spawned = self.spawned.lock().unwrap();
        while *spawned < n.min(MAX_JOBS) {
            let id = *spawned;
            *spawned += 1;
            let me = Arc::clone(self);
            thread::spawn(move || me.worker_loop(id));
        }
    }

    fn worker_loop(self: Arc<Self>, id: usize) {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            let slug = {
                let mut sh = self.shared.lock().unwrap();
                loop {
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let ready = id < sh.jobs && !sh.paused && !sh.queue.is_empty();
                    if ready {
                        break;
                    }
                    let (g, _) = self
                        .cond
                        .wait_timeout(sh, Duration::from_millis(500))
                        .unwrap();
                    sh = g;
                }
                let slug = sh.queue.pop_front().unwrap();
                if let Some(r) = sh.results.get_mut(&slug) {
                    r.status = "building".into();
                    r.worker = id as i64;
                    r.started = now();
                    r.note = "checkout".into();
                }
                slug
            };
            self.active.fetch_add(1, Ordering::Relaxed);
            self.build_one(&slug, id);
            self.active.fetch_sub(1, Ordering::Relaxed);
            self.save_state();
        }
    }

    fn set_result<F: FnOnce(&mut Res)>(&self, slug: &str, f: F) {
        let mut sh = self.shared.lock().unwrap();
        if let Some(r) = sh.results.get_mut(slug) {
            f(r);
        }
    }

    /// Cached exact compiler version (run once per backend/venv — the cohort python matters because
    /// different cohorts can carry different fontmake/gftools versions).
    fn compiler_version(&self, backend: &str, python: &str) -> String {
        let key = (backend.to_string(), python.to_string());
        {
            let sh = self.shared.lock().unwrap();
            if let Some(v) = sh.cver_cache.get(&key) {
                return v.clone();
            }
        }
        let v = compiler_version_str(backend, python, self.cfg.fontc_bin.as_deref());
        let mut sh = self.shared.lock().unwrap();
        sh.cver_cache.entry(key).or_insert_with(|| v.clone());
        v
    }

    fn builder_name(&self) -> String {
        if self.cfg.builder3_bin.is_some() {
            "builder3".into()
        } else {
            "builder2".into()
        }
    }

    /// Cached exact orchestrator version (run once per builder/venv).
    fn builder_version(&self, builder: &str, python: &str) -> String {
        let key = (builder.to_string(), python.to_string());
        {
            let sh = self.shared.lock().unwrap();
            if let Some(v) = sh.bver_cache.get(&key) {
                return v.clone();
            }
        }
        let v = builder_version_str(builder, python, self.cfg.builder3_bin.as_deref());
        let mut sh = self.shared.lock().unwrap();
        sh.bver_cache.entry(key).or_insert_with(|| v.clone());
        v
    }

    /// Append an event (started/built/failed/venv) to events.jsonl — the append-only stream external
    /// web UIs tail. Matches the Python `_emit` shape: {t, type, slug, ...extra}.
    fn emit(&self, etype: &str, slug: &str, extra: serde_json::Value) {
        let mut ev = serde_json::json!({
            "t": (self.elapsed() * 100.0).round() / 100.0, "type": etype, "slug": slug
        });
        if let (Some(obj), Some(ex)) = (ev.as_object_mut(), extra.as_object()) {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        persist::append_event(&self.cfg.build_dir, &ev);
    }

    /// Build the Python-format migration.json (fontc / fontmake-fallback blockers / both-agreement).
    fn migration_json(&self) -> serde_json::Value {
        let sh = self.shared.lock().unwrap();
        let built: Vec<&Res> = sh.results.values().filter(|r| r.status == "built").collect();
        let fontc: Vec<&str> = built.iter().filter(|r| r.backend == "fontc").map(|r| r.slug.as_str()).collect();
        let fm_only: Vec<&str> = built.iter().filter(|r| r.backend == "fontmake").map(|r| r.slug.as_str()).collect();
        let failed: Vec<&str> = sh.results.values().filter(|r| r.status == "failed").map(|r| r.slug.as_str()).collect();
        serde_json::json!({
            "summary": {
                "fontc": fontc.len(),
                "fontmake_only": fm_only.len(),
                "failed": failed.len(),
            },
            "fontc_built": fontc,
            "fontmake_only": fm_only,
            "failed": failed,
        })
    }

    /// Record a family's cohort assignment into the live cohort map (R2 — populates the cohorts view).
    fn note_cohort(&self, slug: &str, cohort: &str, req_text: &str) {
        let mut sh = self.shared.lock().unwrap();
        let members = sh.cohort_members.entry(cohort.to_string()).or_default();
        if !members.iter().any(|m| m == slug) {
            members.push(slug.to_string());
            members.sort();
        }
        sh.cohort_reqs.entry(cohort.to_string()).or_insert_with(|| req_text.to_string());
    }

    fn backend_order(&self) -> Vec<String> {
        let backend = self.shared.lock().unwrap().backend.clone(); // live (editable via config tab)
        match backend.as_str() {
            "fontc" => vec!["fontc".into()],
            "fontmake" => vec!["fontmake".into()],
            "both" => vec!["fontc".into(), "fontmake".into()],
            _ => {
                // auto: fontc-first, fontmake fallback (only if a fontc binary is present)
                if self.cfg.fontc_bin.is_some() {
                    vec!["fontc".into(), "fontmake".into()]
                } else {
                    vec!["fontmake".into()]
                }
            }
        }
    }

    /// Build one family end-to-end. Mirrors the Python `_build` flow (single-backend / auto path).
    fn build_one(&self, slug: &str, _worker: usize) {
        let fam = {
            let sh = self.shared.lock().unwrap();
            match sh.families.get(slug) {
                Some(f) => f.clone(),
                None => return,
            }
        };
        let logname = slug_to_logname(slug);
        let log_path = self.cfg.build_dir.join("logs").join(format!("{}.log", logname));
        let work = self.cfg.build_dir.join("work").join(&logname);
        let out_dir = self.cfg.build_dir.join("out").join(&logname);
        self.set_result(slug, |r| r.log = log_path.to_string_lossy().to_string());

        self.emit("started", slug, serde_json::json!({"worker": _worker}));
        let mirror = mirror_path(&self.cfg.archive, &fam.url);
        if !mirror.is_dir() {
            if !self.cfg.mirror_missing {
                self.fail(slug, "repo not mirrored", &format!("mirror absent: {}", mirror.display()));
                cleanup(&work, self.cfg.keep_work);
                return;
            }
            // --mirror-missing: clone it into the archive (append-only), one clone per repo at a time
            let key = mirror.to_string_lossy().to_string();
            let lock = {
                let mut m = self.clone_locks.lock().unwrap();
                m.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
            };
            let _g = lock.lock().unwrap();
            if !mirror.is_dir() {
                // re-check under the lock (another worker may have just cloned it)
                self.set_result(slug, |r| r.note = "cloning mirror".into());
                if let Err(e) = crate::mirror::clone_mirror(&fam.url, &mirror, 1800, &self.stop, 3) {
                    self.emit("archived", &fam.url, serde_json::json!({"status":"failed","reason":e}));
                    self.fail(slug, "repo unreachable", &format!("mirror clone failed: {}", e));
                    cleanup(&work, self.cfg.keep_work);
                    return;
                }
                self.emit("archived", &fam.url, serde_json::json!({"status":"added"}));
                self.set_result(slug, |r| r.note = String::new());
            }
        }

        // Cohort venv (R2): read the family's requirements read-only from the mirror, create/reuse the
        // shared cohort venv, and build with ITS python. A venv failure (broken deps) fails the family
        // with the cohort error (self-heal will rebuild it on the next start). Without --manage-venvs,
        // every family builds with the single --build-python.
        let python = if let Some(v) = &self.venvs {
            let req = venv::read_requirements_from_mirror(&mirror, &fam.commit);
            self.set_result(slug, |r| r.note = "installing deps".into());
            let (py, cohort, verr) = v.get_python(&req, |_k| {});
            if !verr.is_empty() {
                let msg = format!("venv: {}", verr);
                let (cause, _) = crate::classify::categorize_failure(&msg);
                self.fail(slug, cause, &msg);
                cleanup(&work, self.cfg.keep_work);
                return;
            }
            self.note_cohort(slug, &cohort, &req);
            self.set_result(slug, |r| {
                r.cohort = cohort.clone();
                r.note = String::new();
            });
            py
        } else {
            self.cfg.build_python.clone()
        };

        let order = self.backend_order();
        let builder = self.builder_name();
        let bver = self.builder_version(&builder, &python);
        let mut last_err = String::new();
        let mut fontc_err = String::new();
        let mut built_any = false;

        for (i, backend) in order.iter().enumerate() {
            // fresh extraction for each backend attempt (a previous attempt may have dirtied work/)
            self.set_result(slug, |r| r.note = "checkout".into());
            if let Err(e) = extract_tree(&mirror, &fam.commit, &work, EXTRACT_TIMEOUT, &log_path) {
                last_err = e;
                continue;
            }
            // registered pre-build commands (generate/pre-compile sources) — run AFTER extraction
            // (so they survive the per-backend re-extract) and BEFORE the builder. (R3 / parity)
            if let Some(cmds) = self.build_rules.get(slug) {
                self.set_result(slug, |r| r.note = "pre-build".into());
                log_line(&log_path, &format!("pre-build: running {} command(s)…", cmds.len()));
                if let Err(e) = crate::rules::run_pre_build(&work, &python, cmds, &log_path, self.cfg.timeout) {
                    last_err = e;
                    self.set_result(slug, |r| r.note = String::new());
                    break; // a pre-build failure won't be cured by another backend
                }
                self.set_result(slug, |r| r.note = String::new());
            }
            preclean_outputs(&work);
            let (cfg_path, label) = match resolve_config(self.cfg.google_fonts.as_deref(), &fam, &work) {
                Ok(v) => v,
                Err(e) => {
                    last_err = e;
                    break; // no config -> trying another backend won't help
                }
            };
            let cver = self.compiler_version(backend, &python);
            self.set_result(slug, |r| {
                r.backend = backend.clone();
                r.compiler_version = cver.clone();
                r.builder = builder.clone();
                r.builder_version = bver.clone();
                r.note = String::new();
            });
            log_line(&log_path, &format!(
                "build[{}]: {} {} via {} · config={} — running {}…",
                backend, backend, cver, bver, label, builder
            ));
            let t0 = now();
            let run = run_builder(
                &python,
                &cfg_path,
                &work,
                &log_path,
                self.cfg.timeout,
                backend,
                self.cfg.fontc_bin.as_deref(),
                self.cfg.builder3_bin.as_deref(),
            );
            if let Err(e) = run {
                last_err = format!("{}: {}", backend, e);
                if backend == "fontc" {
                    fontc_err = last_err.clone();
                }
                continue;
            }
            // collect only fonts written during THIS build (mtime gate), recursively
            let (bytes, found, extras) = collect_outputs(&work, &out_dir, &fam.shipped_fonts, t0);
            if !fam.shipped_fonts.is_empty() && found.is_empty() {
                last_err = if extras.is_empty() {
                    format!("{}: produced no expected font files", backend)
                } else {
                    format!("{}: output name mismatch — got {:?}", backend, &extras[..extras.len().min(3)])
                };
                if backend == "fontc" {
                    fontc_err = last_err.clone();
                }
                continue;
            }
            // success
            built_any = true;
            let missing = fam
                .shipped_fonts
                .iter()
                .filter(|f| !found.contains_key(*f))
                .count();
            // optional sha256 vs-shipped comparison (metadata mode, --compare): the Rust-migration
            // signal — did this backend reproduce exactly what GF ships?
            let live_compare = self.shared.lock().unwrap().compare;
            let cmp = if live_compare {
                match &self.cfg.google_fonts {
                    Some(gf) => compare_to_shipped(gf, &fam, &found),
                    None => String::new(),
                }
            } else {
                String::new()
            };
            let used = backend.clone();
            self.set_result(slug, |r| {
                r.status = "built".into();
                r.ended = now();
                r.out_bytes = bytes;
                r.out_missing = missing;
                r.backend = used.clone();
                r.compare = cmp.clone();
                r.note = String::new();
            });
            log_line(&log_path, &format!(
                "DONE: backend={} bytes={} missing={}", backend, bytes, missing
            ));
            self.emit("built", slug, serde_json::json!({"backend": used, "bytes": bytes, "compare": cmp}));
            if !self.cfg.keep_fonts {
                let _ = std::fs::remove_dir_all(&out_dir);
            }
            let _ = i;
            break;
        }

        if !built_any {
            let err = if last_err.is_empty() { "build failed".into() } else { last_err };
            let (cause, _) = crate::classify::categorize_failure(&err);
            self.fail(slug, cause, &err);
            let _ = fontc_err;
        }
        cleanup(&work, self.cfg.keep_work);
    }

    fn fail(&self, slug: &str, cause: &str, msg: &str) {
        // build the durable record WITH M0 provenance (the same data goes to the in-memory history
        // and the append-only failure-history.jsonl, so a restart never loses how a family broke).
        let entry;
        {
            let mut sh = self.shared.lock().unwrap();
            let (backend, cver, builder, bver) = match sh.results.get_mut(slug) {
                Some(r) => {
                    r.status = "failed".into();
                    r.error = msg.chars().take(400).collect();
                    r.ended = now();
                    r.note = String::new();
                    (
                        r.backend.clone(),
                        r.compiler_version.clone(),
                        r.builder.clone(),
                        r.builder_version.clone(),
                    )
                }
                None => return,
            };
            entry = FailHist {
                ts: now(),
                slug: slug.to_string(),
                cause: cause.to_string(),
                error: msg.chars().take(400).collect(),
                backend,
                compiler_version: cver,
                builder,
                builder_version: bver,
            };
            sh.failure_history.push(entry.clone());
            let n = sh.failure_history.len();
            if n > 5000 {
                sh.failure_history.drain(0..n - 5000);
            }
        }
        // append to durable jsonl outside the lock
        persist::append_failure(&self.cfg.build_dir, &entry);
        self.emit("failed", slug, serde_json::json!({"error": msg.chars().take(200).collect::<String>(), "cause": cause}));
        // archive the failing log so a later success can't erase how it broke
        let logname = slug_to_logname(slug);
        let src = self.cfg.build_dir.join("logs").join(format!("{}.log", logname));
        let fdir = self.cfg.build_dir.join("logs").join("failed");
        let _ = std::fs::create_dir_all(&fdir);
        let _ = std::fs::copy(&src, fdir.join(format!("{}.log", logname)));
    }


    fn save_state(&self) {
        let st = {
            let sh = self.shared.lock().unwrap();
            crate::model::StateFile {
                saved_at: now(),
                build_dir: self.cfg.build_dir.to_string_lossy().to_string(),
                elapsed_so_far: self.elapsed(),
                results: sh.results.clone(),
                cohort_members: sh.cohort_members.clone(),
                cohort_reqs: sh.cohort_reqs.clone(),
            }
        };
        persist::write_state_full(&self.cfg.build_dir, &st);
    }

    // ---- live config: a monitor writes control.json; we apply it on the fly ----
    fn spawn_control_watcher(self: &Arc<Self>) {
        let me = Arc::clone(self);
        thread::spawn(move || {
            let mut last = 0u64;
            while !me.stop.load(Ordering::Relaxed) {
                if let Some(ctl) = persist::read_control(&me.cfg.build_dir) {
                    if ctl.seq != last {
                        last = ctl.seq;
                        me.apply_live(&ctl.set);
                    }
                }
                thread::sleep(Duration::from_millis(700));
            }
        });
    }

    /// Apply an untrusted control message (clamped) to the running build.
    pub fn apply_live(self: &Arc<Self>, set: &ControlSet) {
        let mut log = Vec::new();
        let mut new_jobs = None;
        {
            let mut sh = self.shared.lock().unwrap();
            if let Some(j) = set.jobs {
                let j = j.clamp(1, MAX_JOBS);
                sh.jobs = j;
                new_jobs = Some(j);
                log.push(format!("jobs → {}", j));
            }
            if let Some(p) = set.percent {
                let np = p.clamp(0.0, 100.0);
                let old = sh.percent;
                sh.percent = np;
                // R6: raising the percent live enqueues the families newly included in the even
                // sample (fetch + cohort + build them) — the running pool picks them up on notify.
                let mut added = 0;
                if self.cfg.only.trim().is_empty() && np > old {
                    for f in discover::sample_evenly(self.all_families.clone(), np) {
                        if !sh.results.contains_key(&f.slug) {
                            sh.results.insert(
                                f.slug.clone(),
                                Res { slug: f.slug.clone(), status: "queued".into(),
                                      queued_kind: "new".into(), ..Default::default() },
                            );
                            sh.families.insert(f.slug.clone(), f);
                            // (slug pushed after the loop to satisfy the borrow checker)
                            added += 1;
                        }
                    }
                    // collect the freshly-queued slugs and push them onto the work queue
                    let fresh: Vec<String> = sh
                        .results
                        .values()
                        .filter(|r| r.status == "queued" && r.queued_kind == "new"
                            && !sh.queue.contains(&r.slug))
                        .map(|r| r.slug.clone())
                        .collect();
                    for s in fresh {
                        sh.queue.push_back(s);
                    }
                }
                log.push(format!("percent → {:.0} (+{} families)", np, added));
            }
            if let Some(pause) = set.paused {
                sh.paused = pause;
                log.push(if pause { "paused".into() } else { "resumed".into() });
            }
            if let Some(b) = &set.backend {
                sh.backend = b.clone();
                log.push(format!("backend → {}", b));
            }
            if let Some(c) = set.compare {
                sh.compare = c;
                log.push(format!("compare → {}", if c { "on" } else { "off" }));
            }
            if let Some(retry) = &set.retry {
                for slug in retry {
                    if let Some(r) = sh.results.get_mut(slug) {
                        r.status = "queued".into();
                        r.queued_kind = "retry".into();
                        r.error.clear();
                        sh.queue.push_front(slug.clone());
                        log.push(format!("retry {}", slug));
                    }
                }
            }
            if set.retry_all == Some(true) {
                let failed: Vec<String> = sh
                    .results
                    .values()
                    .filter(|r| r.status == "failed")
                    .map(|r| r.slug.clone())
                    .collect();
                for slug in failed {
                    if let Some(r) = sh.results.get_mut(&slug) {
                        r.status = "queued".into();
                        r.queued_kind = "retry".into();
                        r.error.clear();
                        sh.queue.push_back(slug);
                    }
                }
                log.push("retry ALL failed".into());
            }
            for l in &log {
                sh.control_log.push(l.clone());
            }
            let n = sh.control_log.len();
            if n > 200 {
                sh.control_log.drain(0..n - 200);
            }
        }
        if let Some(j) = new_jobs {
            self.ensure_workers(j);
        }
        self.cond.notify_all();
    }

    fn spawn_status_writer(self: &Arc<Self>) {
        let me = Arc::clone(self);
        thread::spawn(move || {
            let mut tick: u64 = 0;
            while !me.stop.load(Ordering::Relaxed) {
                let snap = me.snapshot();
                persist::write_status(&me.cfg.build_dir, &snap);
                if tick % 10 == 0 {
                    // derived report consumed by the dashboard (refreshed every ~10 s, not every tick)
                    persist::write_json_file(&me.cfg.build_dir, "migration.json", &me.migration_json());
                }
                tick += 1;
                thread::sleep(Duration::from_millis(1000));
            }
            // one last write so a monitor sees the final state
            let snap = me.snapshot();
            persist::write_status(&me.cfg.build_dir, &snap);
            persist::write_json_file(&me.cfg.build_dir, "migration.json", &me.migration_json());
        });
    }

    fn spawn_size_thread(self: &Arc<Self>) {
        let me = Arc::clone(self);
        thread::spawn(move || {
            let mut tick: u64 = 0;
            while !me.stop.load(Ordering::Relaxed) {
                let build_total = dir_size(&me.cfg.build_dir);
                let free = free_bytes(&me.cfg.build_dir);
                let cached = cached_cohort_set(&me.cfg.build_dir);
                // The archive (potentially thousands of bare mirrors) changes only during mirroring,
                // and du-ing it is heavy I/O — measure it (+ count its repos) only every ~5 min, not
                // every 10 s. The build dir + cached-venv set, which change constantly, stay at 10 s.
                if tick % 30 == 0 {
                    let (archive_total, nested) = measure_archive(&me.cfg.build_dir, &me.cfg.archive);
                    let arc_count = count_archive(&me.cfg.archive);
                    let mut sh = me.shared.lock().unwrap();
                    sh.disk_archive_total = archive_total;
                    sh.disk_archive_nested = nested;
                    sh.archive_total = arc_count;
                }
                {
                    let mut sh = me.shared.lock().unwrap();
                    sh.disk_build_total = build_total;
                    sh.disk_free = free;
                    sh.cached_cohorts = cached;
                }
                tick += 1;
                for _ in 0..10 {
                    if me.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1000));
                }
            }
        });
    }

    pub fn elapsed(&self) -> f64 {
        self.resumed_elapsed + (now() - self.start_time)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cond.notify_all();
    }

    /// Build the live snapshot rendered by every frontend and written to status.json.
    pub fn snapshot(&self) -> Snapshot {
        let sh = self.shared.lock().unwrap();
        let mut counts = Counts::default();
        let mut backends = Backends::default();
        let mut migration: BTreeMap<String, usize> = BTreeMap::new();
        let mut building = Vec::new();
        let mut queued_list = Vec::new();
        let mut fails = Vec::new();
        let mut built = Vec::new();
        let mut fail_cat: BTreeMap<String, (usize, Vec<String>, &'static str)> = BTreeMap::new();

        for r in sh.results.values() {
            match r.status.as_str() {
                "built" => counts.built += 1,
                "failed" => counts.failed += 1,
                "building" => counts.building += 1,
                "queued" => counts.queued += 1,
                "skipped" => counts.skipped += 1,
                _ => {}
            }
            if r.status == "built" {
                match r.backend.as_str() {
                    "fontc" => {
                        backends.fontc += 1;
                        *migration.entry("fontc".into()).or_default() += 1;
                    }
                    "fontmake" => {
                        backends.fontmake += 1;
                        *migration.entry("fontmake_fallback".into()).or_default() += 1;
                    }
                    _ => {}
                }
                built.push(BuiltItem {
                    slug: r.slug.clone(),
                    backend: r.backend.clone(),
                    bytes: r.out_bytes,
                    compare: r.compare.clone(),
                    log: r.log.clone(),
                    ended: r.ended,
                    compiler_version: r.compiler_version.clone(),
                    builder: r.builder.clone(),
                    builder_version: r.builder_version.clone(),
                });
            }
            if r.status == "building" {
                building.push(BuildingItem {
                    slug: r.slug.clone(),
                    worker: r.worker,
                    dur: now() - r.started,
                    backend: r.backend.clone(),
                    note: r.note.clone(),
                });
            }
            if r.status == "queued" {
                queued_list.push(QueuedItem {
                    slug: r.slug.clone(),
                    kind: if r.queued_kind.is_empty() { "new".into() } else { r.queued_kind.clone() },
                });
            }
            if r.status == "failed" {
                fails.push(FailItem {
                    slug: r.slug.clone(),
                    error: r.error.chars().take(300).collect(),
                    log: r.log.clone(),
                    ended: r.ended,
                    backend: r.backend.clone(),
                    compiler_version: r.compiler_version.clone(),
                    builder: r.builder.clone(),
                    builder_version: r.builder_version.clone(),
                });
                let (cause, hint) = crate::classify::categorize_failure(&r.error);
                let ent = fail_cat.entry(cause.to_string()).or_insert((0, Vec::new(), hint));
                ent.0 += 1;
                if ent.1.len() < 40 {
                    ent.1.push(r.slug.clone());
                }
            }
        }
        building.sort_by(|a, b| a.slug.cmp(&b.slug));
        fails.sort_by(|a, b| b.ended.partial_cmp(&a.ended).unwrap_or(std::cmp::Ordering::Equal));
        built.sort_by(|a, b| b.ended.partial_cmp(&a.ended).unwrap_or(std::cmp::Ordering::Equal));
        fails.truncate(400);
        built.truncate(200);

        let fail_categories = {
            let mut v: Vec<FailCategory> = fail_cat
                .into_iter()
                .map(|(cat, (count, families, hint))| FailCategory {
                    hint: hint.to_string(),
                    cat,
                    count,
                    families,
                })
                .collect();
            v.sort_by(|a, b| b.count.cmp(&a.count));
            v
        };

        let tooling: BTreeMap<String, String> = sh
            .cver_cache
            .iter()
            .filter(|((b, _), _)| b == "fontc" || b == "fontmake")
            .map(|((b, _), v)| (b.clone(), v.clone()))
            .collect();
        let builders: BTreeMap<String, String> =
            sh.bver_cache.iter().map(|((b, _), v)| (b.clone(), v.clone())).collect();

        // cohorts view (R1): from the preserved cohort map; 'cached' = a venv is on disk for that key.
        // Largest cohorts first, matching the Python tool.
        let mut cohorts_out: Vec<CohortView> = sh
            .cohort_members
            .iter()
            .map(|(key, fams)| CohortView {
                key: key.clone(),
                count: fams.len(),
                requirements: sh.cohort_reqs.get(key).cloned().unwrap_or_default(),
                families: fams.clone(),
                cached: sh.cached_cohorts.contains(key),
            })
            .collect();
        cohorts_out.sort_by(|a, b| b.count.cmp(&a.count));

        let mut fail_hist: Vec<FailHist> = sh.failure_history.iter().rev().take(400).cloned().collect();
        fail_hist.reverse();

        // done = nothing queued, nothing building, no worker in flight (correct with 0 families so a
        // daemon idle-exits; the active counter guards the build→built window). Computed before the
        // struct literal moves `counts`.
        let done = counts.queued == 0 && counts.building == 0 && self.active.load(Ordering::Relaxed) == 0;

        let archive = ArchiveView {
            total: sh.archive_total,
            ..Default::default()
        };

        Snapshot {
            elapsed: self.elapsed(),
            disk_used_delta: 0,
            disk_free: sh.disk_free,
            disk_build_total: sh.disk_build_total,
            disk_archive_total: sh.disk_archive_total,
            disk_archive_nested: sh.disk_archive_nested,
            jobs: sh.jobs,
            paused: sh.paused,
            total: sh.results.len(),
            counts,
            backends,
            building,
            failures_recent: fails,
            built_recent: built,
            queued_list,
            fail_categories,
            cohorts_ready: self
                .venvs
                .as_ref()
                .map(|v| v.ready_count())
                .unwrap_or_else(|| cohorts_out.iter().filter(|c| c.cached).count()),
            cohorts: cohorts_out,
            phase: sh.phase.clone(),
            phase_total: sh.library_total,
            phase_done: 0,
            phase_label: sh.phase_label.clone(),
            phase_error: String::new(),
            failure_history: fail_hist,
            tooling,
            builders,
            migration,
            tasks: Vec::new(),
            archive_recent: Vec::new(),
            archive,
            config: {
                // reflect the LIVE (config-tab-editable) values so the form shows current state
                let mut c = config_map(&self.cfg);
                c.insert("jobs".into(), serde_json::json!(sh.jobs));
                c.insert("percent".into(), serde_json::json!(sh.percent));
                c.insert("backend".into(), serde_json::json!(sh.backend));
                c.insert("compare".into(), serde_json::json!(sh.compare));
                c
            },
            control_log: sh.control_log.clone(),
            dep_relaxations: self.venvs.as_ref().map(|v| v.relaxations()).unwrap_or_default(),
            config_path: self.cfg.data_dir.join("gflib-build.config").to_string_lossy().to_string(),
            done,
            daemon_alive: true,
        }
    }
}

// ----------------------------------------------------------------- build subroutines

/// Map a repo URL to its bare-mirror path under the archive (ported from Python `mirror_path`).
pub fn mirror_path(archive: &Path, repo_url: &str) -> PathBuf {
    let mut u = repo_url.trim().trim_end_matches('/').to_string();
    if let Some(rest) = u.strip_prefix("https://") {
        u = rest.to_string();
    } else if let Some(rest) = u.strip_prefix("http://") {
        u = rest.to_string();
    }
    if let Some(idx) = u.find("git@") {
        // git@host:owner/repo -> host/owner/repo
        let tail = &u[idx + 4..];
        u = tail.replacen(':', "/", 1);
    }
    if u.ends_with(".git") {
        u.truncate(u.len() - 4);
    }
    let parts: Vec<&str> = u.split('/').collect();
    if parts.len() >= 2 {
        archive
            .join(parts[parts.len() - 2])
            .join(format!("{}.git", parts[parts.len() - 1]))
    } else {
        archive.join(format!("{}.git", u))
    }
}

fn log_line(log_path: &Path, msg: &str) {
    if let Some(p) = log_path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "{}", msg);
    }
}

/// Stream the pristine tree at `commit` out of the bare mirror with `git archive | tar -x` — a
/// read-only op that never touches the mirror. Returns Err(msg) on failure.
fn extract_tree(mirror: &Path, commit: &str, dest: &Path, _timeout: u64, log_path: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).map_err(|e| format!("mkdir work: {}", e))?;
    log_line(log_path, &format!("extract: git archive {} → {}", commit, dest.display()));
    // git archive --format=tar <commit> | tar -x -C dest
    use std::process::{Command, Stdio};
    let mut git = Command::new("git")
        .args(["--git-dir", &mirror.to_string_lossy(), "archive", "--format=tar", commit])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn git archive: {}", e))?;
    let stdout = git.stdout.take().ok_or("no git stdout")?;
    let tar = Command::new("tar")
        .args(["-x", "-C", &dest.to_string_lossy()])
        .stdin(Stdio::from(stdout))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn tar: {}", e))?;
    let tar_out = tar.wait_with_output().map_err(|e| format!("tar wait: {}", e))?;
    let git_out = git.wait_with_output().map_err(|e| format!("git wait: {}", e))?;
    if !git_out.status.success() {
        return Err(format!(
            "git archive failed: {}",
            String::from_utf8_lossy(&git_out.stderr).trim().chars().take(200).collect::<String>()
        ));
    }
    if !tar_out.status.success() {
        return Err(format!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&tar_out.stderr).trim().chars().take(200).collect::<String>()
        ));
    }
    Ok(())
}

/// Remove committed build outputs so the build regenerates everything from sources.
fn preclean_outputs(work: &Path) {
    for d in OUTPUT_DIRS_TO_CLEAN {
        let p = work.join(d);
        if p.is_dir() {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
    if let Ok(rd) = std::fs::read_dir(work) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("build") && name.ends_with(".ninja") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Resolve the gftools-builder config: a google/fonts override, else the in-repo config_yaml, else
/// an auto-discovered candidate. Returns (config_path, label).
fn resolve_config(google_fonts: Option<&Path>, fam: &Family, work: &Path) -> Result<(PathBuf, String), String> {
    if let Some(gf) = google_fonts {
        let override_cfg = gf.join(&fam.slug).join("config.yaml");
        if override_cfg.is_file() {
            let dest = work.join("__gflib_override_config.yaml");
            std::fs::copy(&override_cfg, &dest).map_err(|e| format!("stage override config: {}", e))?;
            return Ok((dest, format!("override:{}/config.yaml", fam.slug)));
        }
    }
    if !fam.config_yaml.is_empty() {
        let p = work.join(&fam.config_yaml);
        if p.is_file() {
            return Ok((p, fam.config_yaml.clone()));
        }
    }
    for cand in CONFIG_CANDIDATES {
        let p = work.join(cand);
        if p.is_file() {
            return Ok((p, cand.to_string()));
        }
    }
    Err("no config.yaml found (no override, no in-repo config)".into())
}

/// Run the build orchestrator. builder3_bin set -> invoke the Rust-native builder3 binary directly
/// (no Python); else `python -m gftools.builder <config>` (with --experimental-fontc for fontc).
fn run_builder(
    python: &str,
    config_path: &Path,
    work: &Path,
    log_path: &Path,
    timeout: Option<u64>,
    backend: &str,
    fontc_bin: Option<&str>,
    builder3_bin: Option<&str>,
) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let logf = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("open log: {}", e))?;
    let logf2 = logf.try_clone().map_err(|e| format!("clone log fd: {}", e))?;

    let mut cmd;
    let orch;
    if let Some(b3) = builder3_bin {
        cmd = Command::new(b3);
        cmd.arg(config_path);
        orch = "gftools-builder3";
    } else {
        cmd = Command::new(python);
        cmd.args(["-m", "gftools.builder"]).arg(config_path);
        if backend == "fontc" {
            if let Some(fc) = fontc_bin {
                cmd.args(["--experimental-fontc", fc]);
            }
        }
        orch = "gftools.builder";
    }
    log_line(log_path, &format!("===== {} (backend={}) =====", orch, backend));
    cmd.current_dir(work)
        .env("SOURCE_DATE_EPOCH", "0")
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(logf2));

    let mut child = cmd.spawn().map_err(|e| format!("could not launch builder: {}", e))?;
    // simple timeout: poll
    if let Some(t) = timeout {
        let deadline = std::time::Instant::now() + Duration::from_secs(t);
        loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    return if st.success() { Ok(()) } else { Err(last_error_line(log_path)) };
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        return Err(format!("timed out after {}s", t));
                    }
                    thread::sleep(Duration::from_millis(300));
                }
                Err(e) => return Err(format!("wait: {}", e)),
            }
        }
    } else {
        let st = child.wait().map_err(|e| format!("wait: {}", e))?;
        if st.success() {
            Ok(())
        } else {
            Err(last_error_line(log_path))
        }
    }
}

fn last_error_line(log_path: &Path) -> String {
    let txt = std::fs::read_to_string(log_path).unwrap_or_default();
    for ln in txt.lines().rev() {
        let s = ln.trim();
        if !s.is_empty()
            && ["Error", "error", "Exception", "Traceback", "FAILED", "assert"]
                .iter()
                .any(|k| s.contains(k))
        {
            return s.chars().take(200).collect();
        }
    }
    txt.lines()
        .last()
        .map(|l| l.trim().chars().take(200).collect())
        .unwrap_or_else(|| "exit non-zero".into())
}

/// Copy freshly-built fonts (mtime >= cutoff) whose name matches a shipped binary into out_dir.
/// Returns (total_bytes, {name: path}, extras). Recursive — the config may write to any outputDir.
fn collect_outputs(
    work: &Path,
    out_dir: &Path,
    shipped: &[String],
    since: f64,
) -> (u64, BTreeMap<String, PathBuf>, Vec<String>) {
    let _ = std::fs::create_dir_all(out_dir);
    let want: HashSet<&str> = shipped.iter().map(|s| s.as_str()).collect();
    let cutoff = if since > 0.0 { since - 30.0 } else { 0.0 };
    let mut found = BTreeMap::new();
    let mut extras = Vec::new();
    let mut seen = HashSet::new();
    let mut total = 0u64;

    // Scan `work` AND the stray `../fonts` dir: a google/fonts override config.yaml expects to run
    // from sources/ and writes to `../fonts`, so staged at the work root the builder emits to
    // work.parent/fonts — outside the per-family tree. The fresh-mtime + shipped-name filters below
    // keep collecting from the shared dir safe under parallelism. (Parity with the Python fix.)
    let mut stack = vec![work.to_path_buf()];
    if let Some(parent) = work.parent() {
        let stray = parent.join("fonts");
        if stray.is_dir() {
            stack.push(stray);
        }
    }
    let mut fonts = Vec::new();
    while let Some(p) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Some(ext) = path.extension() {
                    let ext = ext.to_string_lossy().to_lowercase();
                    if ext == "ttf" || ext == "otf" {
                        fonts.push(path);
                    }
                }
            }
        }
    }
    fonts.sort();
    for f in fonts {
        let md = match std::fs::metadata(&f) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if cutoff > 0.0 {
            let mt = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if mt < cutoff {
                continue; // a committed/extracted binary, not a fresh build
            }
        }
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        if !want.is_empty() && !want.contains(name.as_str()) {
            if seen.insert(name.clone()) {
                extras.push(name);
            }
            continue;
        }
        if found.contains_key(&name) {
            continue;
        }
        let dst = out_dir.join(&name);
        if std::fs::copy(&f, &dst).is_ok() {
            total += std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
            found.insert(name, dst);
        }
    }
    (total, found, extras)
}

fn cleanup(work: &Path, keep: bool) {
    if !keep {
        let _ = std::fs::remove_dir_all(work);
    }
}

/// sha256 of a file via `sha256sum` (coreutils — dependency-free). None on failure.
fn sha256_file(path: &Path) -> Option<String> {
    let out = std::process::Command::new("sha256sum").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
}

/// Compare built fonts to the binaries GF ships (in the family dir): identical / differ / missing.
/// Ported from the Python `compare_to_shipped`.
fn compare_to_shipped(google_fonts: &Path, fam: &Family, built: &BTreeMap<String, PathBuf>) -> String {
    if fam.shipped_fonts.is_empty() {
        return String::new();
    }
    let fam_dir = google_fonts.join(&fam.slug);
    let mut all_identical = true;
    let mut any_present = false;
    for fname in &fam.shipped_fonts {
        let refp = fam_dir.join(fname);
        if !refp.is_file() {
            continue;
        }
        let b = match built.get(fname) {
            Some(b) => b,
            None => return "missing".into(),
        };
        any_present = true;
        if sha256_file(&refp) != sha256_file(b) {
            all_identical = false;
        }
    }
    if any_present {
        if all_identical {
            "identical".into()
        } else {
            "differ".into()
        }
    } else {
        "missing".into()
    }
}

/// Count repos in the whole archive on disk ({owner}/{repo}.git).
fn count_archive(archive: &Path) -> usize {
    let mut n = 0;
    if let Ok(owners) = std::fs::read_dir(archive) {
        for owner in owners.flatten() {
            if owner.path().is_dir() {
                if let Ok(repos) = std::fs::read_dir(owner.path()) {
                    for r in repos.flatten() {
                        if r.path().extension().map(|e| e == "git").unwrap_or(false) {
                            n += 1;
                        }
                    }
                }
            }
        }
    }
    n
}

/// Archive (bytes, nested?). Returns nested=true (and 0 bytes) when the archive lives under build_dir
/// — it's already in the build total, so the header notes it's included rather than double-counting.
fn measure_archive(build_dir: &Path, archive: &Path) -> (u64, bool) {
    let bd = build_dir.canonicalize().unwrap_or_else(|_| build_dir.to_path_buf());
    let ar = match archive.canonicalize() {
        Ok(p) => p,
        Err(_) => return (0, false),
    };
    if ar == bd || ar.starts_with(&bd) {
        return (0, true);
    }
    (dir_size(&ar), false)
}

/// Cohort keys with a venv on disk (a `venvs/<key>/.gflib-installed` success marker) — the 'cached'
/// flag in the cohorts view. Scanned off the render path (in the size thread), like the Python tool.
fn cached_cohort_set(build_dir: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let vroot = build_dir.join("venvs");
    if let Ok(rd) = std::fs::read_dir(&vroot) {
        for e in rd.flatten() {
            if e.path().join(".gflib-installed").is_file() {
                set.insert(e.file_name().to_string_lossy().to_string());
            }
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mirror_path_maps_urls() {
        let ar = Path::new("/arch");
        assert_eq!(mirror_path(ar, "https://github.com/googlefonts/foo"), Path::new("/arch/googlefonts/foo.git"));
        assert_eq!(mirror_path(ar, "https://github.com/googlefonts/foo.git"), Path::new("/arch/googlefonts/foo.git"));
        assert_eq!(mirror_path(ar, "git@github.com:owner/bar.git"), Path::new("/arch/owner/bar.git"));
    }
}

#[cfg(test)]
mod compare_tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn compare_identical_differ_missing() {
        let root = std::env::temp_dir().join(format!("_cmp_{}", std::process::id()));
        let gf = root.join("gf");
        let slug = "ofl/x";
        let fam_dir = gf.join(slug);
        std::fs::create_dir_all(&fam_dir).unwrap();
        std::fs::write(fam_dir.join("X.ttf"), b"FONTDATA").unwrap();
        let fam = Family { slug: slug.into(), shipped_fonts: vec!["X.ttf".into()], ..Default::default() };

        // identical: built byte-for-byte equal to shipped
        let bdir = root.join("built");
        std::fs::create_dir_all(&bdir).unwrap();
        let bp = bdir.join("X.ttf");
        std::fs::write(&bp, b"FONTDATA").unwrap();
        let mut built = BTreeMap::new();
        built.insert("X.ttf".to_string(), bp.clone());
        assert_eq!(compare_to_shipped(&gf, &fam, &built), "identical");

        // differ: built differs
        std::fs::write(&bp, b"OTHERDATA").unwrap();
        assert_eq!(compare_to_shipped(&gf, &fam, &built), "differ");

        // missing: built lacks a shipped font
        let empty: BTreeMap<String, PathBuf> = BTreeMap::new();
        assert_eq!(compare_to_shipped(&gf, &fam, &empty), "missing");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_outputs_scans_stray_override_fonts_dir() {
        // an override config.yaml writes to ../fonts (work.parent/fonts) — must be collected
        let root = std::env::temp_dir().join(format!("_stray_{}", std::process::id()));
        let work = root.join("work").join("ofl__demo");
        let stray = root.join("work").join("fonts").join("ttf");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("Demo[wght].ttf"), b"FRESHFONT").unwrap();
        let out = root.join("out");
        let since = crate::util::now() - 1.0;
        let (total, found, _extras) =
            collect_outputs(&work, &out, &["Demo[wght].ttf".to_string()], since);
        assert!(found.contains_key("Demo[wght].ttf"), "stray ../fonts output must be collected");
        assert!(total > 0 && out.join("Demo[wght].ttf").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
