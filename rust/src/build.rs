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
/// Marker comment every gflib-build-authored override config.yaml carries (in the google/fonts build
/// clone). The "retry override-fixed" UI action re-queues failed families whose override has it.
pub const OVERRIDE_MARKER: &str = "# gflib-build override";

/// Signature of a family's effective gflib-build fix: a hash of the override config.yaml text plus its
/// build_rules pre-build entry. Empty unless the config carries OVERRIDE_MARKER **or** the family has a
/// build_rules entry — so only fixes WE authored are tracked (never a natural upstream config.yaml), but a
/// build_rules-only fix (a pre-build / source patch with no override config) still auto-rebuilds. The
/// config-watcher re-queues a failed family when this changes — editing the override OR its pre-build flips it.
pub fn config_signature(config_text: &str, rule: Option<&Vec<String>>) -> String {
    if !config_text.contains(OVERRIDE_MARKER) && rule.is_none() {
        return String::new();
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    config_text.hash(&mut h);
    rule.hash(&mut h);
    format!("{:016x}", h.finish())
}

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
    pub orchestrator: String, // SETTING (live): auto | builder3 | builder2 — drives attempt_chain
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
    // archive pre-warmer (R3): proactively mirror the whole worklist's repos, concurrent with the
    // build, so the archive reaches 100% regardless of build pace (matches Python's populate_archive)
    pub archive_pending: VecDeque<String>,    // repo URLs queued to mirror (de-duped, mirror absent)
    pub archive_active: Vec<String>,          // repo URLs cloning right now
    pub archive_recent: Vec<ArchiveRecent>,   // recently mirrored / failed (newest last, capped)
    // fontspector QA orchestration (--fontspector): QA green families asynchronously, niced, during
    // the build; the live aggregate feeds the breakdown panels.
    pub qa_queue: VecDeque<String>,           // green families awaiting QA
    pub qa_active: Vec<String>,               // families being QA'd right now
    pub qa_done: HashSet<String>,             // families QA'd this run (de-dup)
    pub qa_init: bool,                        // QA subsystem ready (binary resolved + queue seeded, or disabled)
    pub build_debs: bool,                     // SETTING (live): auto-draft+build .debs for built families
    pub packaged: HashSet<String>,            // families already packaged this session (de-dup; cleared on rebuild)
    pub pkg_now: String,                      // package worker's current activity ("packaging <slug>" / "linting <slug>" / "")
    pub fontspector: Option<crate::model::FontspectorView>, // live aggregate (also written to _summary.json)
    // cohort map (R1: preserved across resume/migration; populated by R2's VenvManager later)
    pub cohort_members: BTreeMap<String, Vec<String>>,
    pub cohort_reqs: BTreeMap<String, String>,
    pub cached_cohorts: HashSet<String>,
    pub reset_portions: Vec<crate::model::ResetPortion>, // reset-tab portions + live sizes
    pub reset_progress: BTreeMap<String, (u64, u64)>, // in-flight deletions: key -> (freed, total)
    pub reset_notes: BTreeMap<String, (String, f64)>, // transient per-portion outcome: key -> (msg, set_at) // cohorts with a venv on disk (off-thread; for the 'cached' flag)
    pub op_stats: HashMap<String, (f64, usize, f64)>, // op -> (total_secs, count, max) for timings.json
}

/// One in-flight builder process group and whether it is currently SIGSTOP-frozen. Kept in start
/// order (push appends) so the regulator can freeze the NEWEST excess and thaw the OLDEST first.
struct RunEntry {
    pgid: i32,
    frozen: bool,
    slug: String, // so the UI can mark WHICH in-flight builds are frozen
}

/// The registry of in-flight builder children, with per-build freeze state. Replaces the old
/// `HashSet<i32>`: the same membership/iteration, plus the ordering + per-entry `frozen` flag the
/// job regulator needs to keep exactly `jobs` builds actively running (the rest frozen, not killed).
#[derive(Default)]
pub struct RunReg {
    entries: Vec<RunEntry>,
}

impl RunReg {
    fn insert(&mut self, pgid: i32, frozen: bool, slug: String) {
        self.entries.push(RunEntry { pgid, frozen, slug });
    }
    /// Slugs of the currently SIGSTOP-frozen in-flight builds (job limit lowered / paused).
    fn frozen_slugs(&self) -> std::collections::HashSet<String> {
        self.entries.iter().filter(|e| e.frozen).map(|e| e.slug.clone()).collect()
    }
    fn remove(&mut self, pgid: i32) {
        self.entries.retain(|e| e.pgid != pgid);
    }
    fn len(&self) -> usize {
        self.entries.len()
    }
    fn unfrozen(&self) -> usize {
        self.entries.iter().filter(|e| !e.frozen).count()
    }
    fn frozen_count(&self) -> usize {
        self.entries.iter().filter(|e| e.frozen).count()
    }
    fn is_frozen(&self, pgid: i32) -> bool {
        self.entries.iter().any(|e| e.pgid == pgid && e.frozen)
    }
    fn pgids(&self) -> Vec<i32> {
        self.entries.iter().map(|e| e.pgid).collect()
    }
    /// Decide which builds to freeze/thaw to reach the target (paused → all frozen; otherwise exactly
    /// `jobs` unfrozen). Mutates the per-entry `frozen` flags and RETURNS the pgids to SIGSTOP / SIGCONT
    /// so the caller can signal them (under the same lock). Freezes the NEWEST excess (tail-first) and
    /// thaws the OLDEST frozen (head-first), so the builds closest to finishing keep running.
    fn plan(&mut self, paused: bool, jobs: usize) -> (Vec<i32>, Vec<i32>) {
        let (mut freeze, mut thaw) = (Vec::new(), Vec::new());
        if paused {
            for e in self.entries.iter_mut() {
                if !e.frozen {
                    e.frozen = true;
                    freeze.push(e.pgid);
                }
            }
            return (freeze, thaw);
        }
        // jobs == 0 is DRAIN, not pause: the worker ready-gate (id < jobs) stops NEW builds starting, but
        // every in-flight build must run to COMPLETION. So thaw any build that a prior jobs-cut or pause
        // left SIGSTOP-frozen (else it would sit suspended forever — never draining), and freeze nothing.
        if jobs == 0 {
            for e in self.entries.iter_mut() {
                if e.frozen {
                    e.frozen = false;
                    thaw.push(e.pgid);
                }
            }
            return (freeze, thaw);
        }
        let mut unfrozen = self.entries.iter().filter(|e| !e.frozen).count();
        if unfrozen > jobs {
            for e in self.entries.iter_mut().rev() {
                if unfrozen <= jobs {
                    break;
                }
                if !e.frozen {
                    e.frozen = true;
                    freeze.push(e.pgid);
                    unfrozen -= 1;
                }
            }
        }
        if unfrozen < jobs {
            for e in self.entries.iter_mut() {
                if unfrozen >= jobs {
                    break;
                }
                if e.frozen {
                    e.frozen = false;
                    thaw.push(e.pgid);
                    unfrozen += 1;
                }
            }
        }
        (freeze, thaw)
    }
}

pub struct Orchestrator {
    pub cfg: Config,
    pub shared: Arc<Mutex<Shared>>,
    pub cond: Arc<Condvar>,
    pub stop: Arc<AtomicBool>,
    pub restart_requested: Arc<AtomicBool>, // UI "Restart": graceful-stop then re-spawn (see run_daemon)
    pub start_time: f64,
    pub resumed_elapsed: f64,
    pub active: AtomicUsize,
    pub reset_all_running: AtomicBool, // a "delete everything" wipe is in progress — reject re-entry
    // Set by "delete everything" to ABORT in-flight builds: build_one bails between attempts instead of
    // advancing to the next (builder,backend) pair after its compile is SIGKILLed, and the venv installer
    // kills its pip subprocess. Shared (Arc) so the VenvManager can read it. Distinct from `stop`, which
    // exits the daemon — this just unwinds the current builds while the daemon stays alive (paused).
    pub abort_builds: Arc<AtomicBool>,
    pub spawned: Mutex<usize>,
    pub venvs: Option<VenvManager>, // cohort venv manager (R2); None unless --manage-venvs
    pub build_rules: std::collections::HashMap<String, Vec<String>>, // per-family pre-build (R3)
    pub all_families: Vec<Family>,  // full discovered list (R6: raising % enqueues more from here)
    pub clone_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>, // per-repo clone lock (--mirror-missing)
    pub qa_bin: Mutex<Option<(PathBuf, String)>>, // resolved fontspector (path, version), lazily set
    pub dry_playbook: HashMap<String, Res>, // --dry-run: each family's previous outcome to replay
    pub running: Mutex<RunReg>,             // in-flight builder children + freeze state (freeze/thaw/kill)
    pub frozen: AtomicBool,                 // global pause SIGSTOPs ALL running builds; mirror of sh.paused
    pub job_limit: AtomicUsize,             // live jobs target; lock-free mirror of sh.jobs for the regulator
    pub crater: Option<crate::crater::CraterData>, // fontc_crater latest verdict (None = unavailable/disabled)
    pub crater_by_slug: BTreeMap<String, crate::crater::CraterStatus>, // family slug -> crater verdict
    pub toolchain: Arc<crate::toolchain::Toolchain>, // fontc + builder3 ready-gate (resolved at start())
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
            // discovery keeps its own parallelism even when build jobs==0 (inspect-only)
            "archive" => discover::discover_archive(&cfg.archive, &cfg.archive_rev, cfg.jobs.max(1), want.as_ref()),
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
        // dry-run MOCKUP: remember each family's previous outcome, then re-queue EVERY family so the
        // dashboard animates the whole build again — but the worker just replays it (no real work).
        let dry_playbook: HashMap<String, Res> =
            if cfg.dry_run { prior.iter().map(|(k, v)| (k.clone(), v.clone())).collect() } else { HashMap::new() };

        // fontc_crater comparison: load the latest per-target verdict and join each discovered family
        // to it by upstream repo. Done before the worklist loop so a --retrigger-crater selection can
        // force-rebuild families by crater verdict.
        let crater = if cfg.no_crater {
            None
        } else {
            crate::crater::resolve_path(cfg.crater_path.as_deref(), &cfg.data_dir)
                .and_then(|p| crate::crater::load(&p))
        };
        let crater_by_slug: BTreeMap<String, crate::crater::CraterStatus> = match &crater {
            Some(c) => fams
                .iter()
                .filter_map(|f| c.status_for_url(&f.url).map(|st| (f.slug.clone(), st.clone())))
                .collect(),
            None => BTreeMap::new(),
        };
        // retrigger set: explicit --retrigger slugs ∪ families matching --retrigger-crater. These are
        // force-rebuilt below regardless of their prior built/failed status (the "I just applied a
        // fix, rebuild the affected families" path).
        let mut retrigger_set: HashSet<String> = cfg
            .retrigger
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !cfg.retrigger_crater.trim().is_empty() {
            let mode = cfg.retrigger_crater.trim().to_lowercase();
            use crate::crater::CraterStatus as CS;
            for (slug, st) in &crater_by_slug {
                let hit = match mode.as_str() {
                    "fontc-failed" | "fontc_failed" => matches!(st, CS::FontcFailed),
                    "both-failed" | "both_failed" => matches!(st, CS::BothFailed),
                    "failed" => st.fontc_failed(),
                    "diff" => matches!(st, CS::Diff(_)),
                    _ => false,
                };
                if hit {
                    retrigger_set.insert(slug.clone());
                }
            }
        }

        let mut results = BTreeMap::new();
        let mut families = BTreeMap::new();
        // (slug, prior_duration) for queued families — sorted longest-first below to shrink the tail
        let mut queued_with_dur: Vec<(String, f64, bool, bool)> = Vec::new(); // (slug, prior_dur, requested_rebuild, is_upgrade)
        for f in fams {
            let slug = f.slug.clone();
            families.insert(slug.clone(), f);
            let prev = prior.get(&slug);
            let force = retrigger_set.contains(&slug);
            // resume: keep a prior success unless --rebuild; re-queue a failure if the user forces it
            // OR (self-heal, matching Python) its cause is in the AUTO_RETRY set — a fresh attempt can
            // clear a rebuilt venv / retried clone / updated mirror, so the failure hints stay honest.
            // A --retrigger / --retrigger-crater hit force-rebuilds regardless of prior status.
            let (status, kind) = if cfg.dry_run {
                ("queued", "new") // re-queue all for the mockup replay
            } else if force {
                ("queued", "rebuild")
            } else { match prev {
                Some(p) if p.status == "built" && !cfg.rebuild => {
                    // auto-upgrade: a kept success below the top rung (fontmake, or fontc under
                    // builder2) is re-attempted at the better rungs — automatically, exactly once
                    // per toolchain signature (pins + orchestrator). The prior result is restored
                    // if the upgrade fails, so this never costs an existing success.
                    // compare against the FULLY-CAPABLE signature: tool availability isn't known
                    // yet (the resolver runs at start()), so enqueue optimistically — build_one
                    // no-ops cheaply (quick restore, no extraction) when the actual capabilities
                    // turn out to match what this result was already attempted under.
                    let full_sig = crate::toolchain::run_sig(
                        &cfg.orchestrator, true, cfg.orchestrator != "builder2");
                    let upgradable = cfg.auto_upgrade
                        && cfg.backend != "fontmake"
                        && cfg.backend != "both"
                        && p.backend != "both"
                        && result_rung(&p.builder, &p.backend) < 2
                        && p.upgrade_attempted != full_sig;
                    if upgradable {
                        ("queued", "upgrade")
                    } else {
                        ("built", "")
                    }
                }
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
            }};
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
                // `force` is an explicit --retrigger of THIS family (a subset request); --rebuild rebuilds
                // everything, so it isn't a per-family priority signal.
                let requested = force;
                r.queued_kind = if cfg.rebuild || force { "rebuild".into() } else { kind.into() };
                queued_with_dur.push((slug.clone(), prior_dur, requested, kind == "upgrade"));
            }
            results.insert(slug, r);
        }
        // requested rebuilds (--retrigger) first; then longest-first (known long prior build) so big
        // families parallelize early; never-built (dur 0) last. Auto-UPGRADES go at the very end:
        // improving an existing success must never delay first-time coverage or a real retry.
        queued_with_dur.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then(a.3.cmp(&b.3)) // non-upgrades before upgrades
                .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
        });
        let queue: VecDeque<String> = queued_with_dur.into_iter().map(|(s, _, _, _)| s).collect();

        let failure_history = persist::read_failure_history(&cfg.build_dir);
        // jobs 0 = inspect-only: keep the loaded data + UI live but spawn NO build workers.
        let jobs = cfg.jobs.min(MAX_JOBS);
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
            orchestrator: cfg.orchestrator.clone(),
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
            archive_pending: VecDeque::new(),
            archive_active: Vec::new(),
            archive_recent: Vec::new(),
            qa_queue: VecDeque::new(),
            qa_active: Vec::new(),
            qa_done: HashSet::new(),
            qa_init: false,
            build_debs: cfg.build_debs,
            packaged: HashSet::new(),
            pkg_now: String::new(),
            fontspector: None,
            cohort_members: state.cohort_members,
            cohort_reqs: state.cohort_reqs,
            cached_cohorts: HashSet::new(),
            reset_portions: Vec::new(),
            reset_progress: BTreeMap::new(),
            reset_notes: BTreeMap::new(),
            op_stats: HashMap::new(),
        };
        let abort_builds = Arc::new(AtomicBool::new(false));
        let venvs = if cfg.manage_venvs {
            Some(VenvManager::new(&cfg.build_dir, &cfg.pythons, cfg.base_requirements.clone(), abort_builds.clone()))
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
            restart_requested: Arc::new(AtomicBool::new(false)),
            start_time: now(),
            resumed_elapsed: state.elapsed_so_far, // cumulative clock survives restart/migration (R1)
            active: AtomicUsize::new(0),
            reset_all_running: AtomicBool::new(false),
            abort_builds,
            spawned: Mutex::new(0),
            venvs,
            build_rules,
            all_families,
            clone_locks: Mutex::new(HashMap::new()),
            qa_bin: Mutex::new(None),
            dry_playbook,
            running: Mutex::new(RunReg::default()),
            frozen: AtomicBool::new(false),
            job_limit: AtomicUsize::new(jobs),
            crater,
            crater_by_slug,
            toolchain: Arc::new(crate::toolchain::Toolchain::default()),
        })
    }

    /// Spawn the worker pool, the status writer, the disk-size thread, and the control watcher.
    pub fn start(self: &Arc<Self>) {
        let _ = std::fs::create_dir_all(&self.cfg.build_dir);
        let _ = std::fs::create_dir_all(self.cfg.build_dir.join("logs"));
        // --dry-run MOCKUP: no real work — show the loaded QA results, replay the build (looping), and
        // skip the base venv, the archive pre-warmer and real fontspector.
        if self.cfg.dry_run {
            // read-only: load the saved QA summary, never WRITE state.json/status.json (so the real
            // session's durable resume state is preserved). The demo is served in-process (foreground).
            self.shared.lock().unwrap().fontspector =
                crate::persist::read_fontspector_summary(&self.cfg.build_dir);
            let jobs = self.shared.lock().unwrap().jobs;
            self.ensure_workers(jobs);
            self.spawn_dry_run_loop();
            self.spawn_size_thread();
            self.spawn_control_watcher();
            return;
        }
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
        self.spawn_toolchain_resolver();
        let jobs = self.shared.lock().unwrap().jobs;
        self.ensure_workers(jobs);
        self.spawn_archive_prewarmer();
        self.spawn_qa();
        self.spawn_package_worker(); // parks until build_debs is on; then packages built families live
        self.spawn_status_writer();
        self.spawn_size_thread();
        self.spawn_control_watcher();
        self.spawn_config_watcher();
    }

    /// Resolve fontc + builder3 off-thread (explicit flag → provisioned pin → auto-provision →
    /// detected) and publish the verdicts to the toolchain ready-gate. Workers entering build_one
    /// wait on the gate; a tool that can't be resolved is Unavailable and the per-family attempt
    /// chain simply degrades past it (builder2 / fontmake) — provisioning failure never blocks the
    /// run. First-run provisioning compiles builder3 (~700 crates) / fontc, which takes minutes; the
    /// progress is visible as pipeline tasks and in the control log; later runs hit the cached pins.
    fn spawn_toolchain_resolver(self: &Arc<Self>) {
        use crate::toolchain as tc;
        let me = Arc::clone(self);
        thread::spawn(move || {
            // drop-guard: if this thread unwinds (or returns early), anything still Pending gets
            // an Unavailable verdict — workers waiting on the gate are NEVER stranded.
            struct Guard(Arc<tc::Toolchain>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.resolve_pending("resolver aborted");
                }
            }
            let _guard = Guard(Arc::clone(&me.toolchain));
            // a persisted-but-stale explicit path must not hard-fail the tool: soften it to None so
            // the pin engages (the control log explains). A LIVE explicit path is honored verbatim.
            let soften = |bin: &Option<String>, name: &str, me: &Arc<Orchestrator>| -> Option<String> {
                match bin {
                    Some(b) if Path::new(b).is_file() => Some(b.clone()),
                    Some(b) => {
                        me.shared.lock().unwrap().control_log.push(format!(
                            "{}: configured binary missing ({}) — resolving automatically", name, b));
                        None
                    }
                    None => None,
                }
            };
            let tools_root = me.cfg.data_dir.join("tools");
            let explicit_fc = soften(&me.cfg.fontc_bin, "fontc", &me);
            let fc = tc::ensure_tool(
                &tc::fontc_spec(), explicit_fc.as_deref(), &tools_root, me.cfg.auto_provision,
                Some(&me.stop), crate::discover::detect_fontc,
            );
            me.log_tool_verdict("fontc", &fc);
            me.toolchain.set_fontc(fc);

            // orchestrator=builder2 → skip builder3 entirely (no provisioning, gate = Unavailable)
            let b3 = if me.cfg.orchestrator == "builder2" {
                tc::ToolStatus::Unavailable("disabled (--orchestrator builder2)".into())
            } else {
                let explicit_b3 = soften(&me.cfg.builder3_bin, "builder3", &me);
                tc::ensure_tool(
                    &tc::builder3_spec(), explicit_b3.as_deref(), &tools_root, me.cfg.auto_provision,
                    Some(&me.stop), tc::detect_builder3,
                )
            };
            me.log_tool_verdict("builder3", &b3);
            me.toolchain.set_builder3(b3);
        });
    }

    fn log_tool_verdict(&self, name: &str, st: &crate::toolchain::ToolStatus) {
        use crate::toolchain::ToolStatus;
        let line = match st {
            ToolStatus::Ready { path, source } => format!("{}: {} ({})", name, path, source),
            ToolStatus::Unavailable(e) => format!("{}: UNAVAILABLE — {}", name, e),
            ToolStatus::Pending => return,
        };
        self.shared.lock().unwrap().control_log.push(line);
    }

    /// --dry-run: when the replayed build finishes, pause a few seconds (so the 'complete' state shows)
    /// then re-queue every family and replay again — continuous demo activity, hands-free.
    fn spawn_dry_run_loop(self: &Arc<Self>) {
        let me = Arc::clone(self);
        thread::spawn(move || loop {
            if me.stop.load(Ordering::Relaxed) {
                return;
            }
            let idle = {
                let sh = me.shared.lock().unwrap();
                sh.queue.is_empty()
            } && me.active.load(Ordering::Relaxed) == 0;
            if idle {
                thread::sleep(Duration::from_secs(6)); // let the 'complete' banner show
                if me.stop.load(Ordering::Relaxed) {
                    return;
                }
                let mut sh = me.shared.lock().unwrap();
                let slugs: Vec<String> = sh.results.keys().cloned().collect();
                for s in slugs {
                    if let Some(r) = sh.results.get_mut(&s) {
                        r.status = "queued".into();
                        r.note = String::new();
                    }
                    sh.queue.push_back(s);
                }
                drop(sh);
                me.cond.notify_all();
            }
            thread::sleep(Duration::from_millis(800));
        });
    }

    /// Asynchronous fontspector QA (--fontspector): resolve the binary off-thread (may cargo-install),
    /// seed the queue with already-built families, then run niced QA workers + a periodic aggregator.
    fn spawn_qa(self: &Arc<Self>) {
        if !self.cfg.fontspector_qa {
            return;
        }
        let me = Arc::clone(self);
        thread::spawn(move || {
            // resolve the pinned binary (one-time; may compile via cargo install — off the hot path)
            match crate::fontspector::ensure_binary(&me.cfg) {
                Ok(bv) => {
                    *me.qa_bin.lock().unwrap() = Some(bv);
                }
                Err(e) => {
                    let mut sh = me.shared.lock().unwrap();
                    sh.control_log.push(format!("fontspector QA disabled: {}", e));
                    sh.qa_init = true; // don't block the daemon's done-state when QA can't run
                    return;
                }
            }
            let _ = std::fs::create_dir_all(crate::persist::fontspector_dir(&me.cfg.build_dir));
            // seed the queue with every already-built family not yet QA'd
            me.seed_qa_queue();
            me.shared.lock().unwrap().qa_init = true;
            // a small niced QA pool that yields to build workers
            let n = me.cfg.jobs.clamp(1, 8);
            for _ in 0..n {
                let w = Arc::clone(&me);
                thread::spawn(move || w.qa_worker_loop());
            }
            let agg = Arc::clone(&me);
            thread::spawn(move || agg.qa_aggregator_loop());
        });
    }

    /// Enqueue every family that has built output but no stored QA result yet.
    fn seed_qa_queue(&self) {
        let fsdir = crate::persist::fontspector_dir(&self.cfg.build_dir);
        let built = crate::fontspector::enumerate_built(&self.cfg.build_dir.join("out"));
        let mut sh = self.shared.lock().unwrap();
        for (slug, _fonts) in built {
            let has = fsdir.join(format!("{}.json", slug.replace('/', "__"))).is_file();
            if (!has || self.cfg.fontspector_rerun) && !sh.qa_done.contains(&slug) && !sh.qa_queue.contains(&slug) {
                sh.qa_queue.push_back(slug);
            }
        }
    }

    /// Enqueue one freshly-green family for QA (called on every successful build).
    fn enqueue_qa(&self, slug: &str) {
        if !self.cfg.fontspector_qa {
            return;
        }
        let mut sh = self.shared.lock().unwrap();
        if !sh.qa_done.contains(slug) && !sh.qa_queue.contains(&slug.to_string()) {
            sh.qa_queue.push_back(slug.to_string());
        }
    }

    fn qa_worker_loop(self: Arc<Self>) {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            if self.frozen.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(300)); // paused: don't start new QA runs (free CPU/RAM)
                continue;
            }
            let slug = {
                let mut sh = self.shared.lock().unwrap();
                match sh.qa_queue.pop_front() {
                    Some(s) => {
                        sh.qa_active.push(s.clone());
                        s
                    }
                    None => {
                        drop(sh);
                        thread::sleep(Duration::from_millis(500)); // idle: wait for more green families
                        continue;
                    }
                }
            };
            let bin = self.qa_bin.lock().unwrap().clone();
            if let Some((path, version)) = bin {
                let fonts = crate::fontspector::collect_fonts(&self.cfg.build_dir.join("out").join(slug.replace('/', "__")));
                if !fonts.is_empty() {
                    // niced fontspector so it yields to the build workers
                    if let Ok(res) = crate::fontspector::run_one(&path, &self.cfg, &slug, &fonts, &version, true) {
                        let resfile = crate::persist::fontspector_dir(&self.cfg.build_dir).join(format!("{}.json", slug.replace('/', "__")));
                        let _ = std::fs::write(&resfile, serde_json::to_string(&res.json).unwrap_or_default());
                    }
                }
            }
            let mut sh = self.shared.lock().unwrap();
            sh.qa_active.retain(|s| s != &slug);
            sh.qa_done.insert(slug);
        }
    }

    /// Re-aggregate the per-family results into the live snapshot + _summary.json every few seconds.
    fn qa_aggregator_loop(self: Arc<Self>) {
        let fsdir = crate::persist::fontspector_dir(&self.cfg.build_dir);
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            // park while frozen (global pause / a "delete everything" wipe) so we don't recreate
            // _summary.json under the deletion — mirrors the build/QA/package workers' freeze-gate
            if self.frozen.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            let (path, version) = match self.qa_bin.lock().unwrap().clone() {
                Some(bv) => bv,
                None => { thread::sleep(Duration::from_secs(2)); continue; }
            };
            let _ = path;
            let view = crate::fontspector::aggregate(&fsdir, &self.cfg.fontspector_profile, &version);
            let _ = std::fs::write(fsdir.join("_summary.json"), serde_json::to_string(&view).unwrap_or_default());
            self.shared.lock().unwrap().fontspector = Some(view);
            thread::sleep(Duration::from_secs(4));
        }
    }

    /// Proactively mirror EVERY worklist repo into the archive, concurrent with the build, so the
    /// archive reaches 100% regardless of build pace (a port of Python's `populate_archive`
    /// pre-warmer). Build workers and the pre-warmer share `clone_locks`, so no repo is cloned twice.
    fn spawn_archive_prewarmer(self: &Arc<Self>) {
        if !self.cfg.mirror_missing {
            return; // only when allowed to add to the archive (append-only)
        }
        // de-duped, sorted set of worklist repo URLs whose mirror is not yet on disk
        let mut seen = HashSet::new();
        let mut urls: Vec<String> = Vec::new();
        for f in &self.all_families {
            if f.url.is_empty() || !seen.insert(f.url.clone()) {
                continue;
            }
            if !mirror_path(&self.cfg.archive, &f.url).is_dir() {
                urls.push(f.url.clone());
            }
        }
        urls.sort();
        if urls.is_empty() {
            return; // the archive already covers the whole worklist
        }
        {
            let mut sh = self.shared.lock().unwrap();
            sh.archive_pending = urls.into_iter().collect();
        }
        // a modest pool so the pre-warmer's clones don't crowd out the build workers' clones
        let n = self.cfg.jobs.clamp(1, 6);
        for _ in 0..n {
            let me = Arc::clone(self);
            thread::spawn(move || me.prewarm_loop());
        }
    }

    fn prewarm_loop(self: Arc<Self>) {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            if self.frozen.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(300)); // paused: don't start new archive clones
                continue;
            }
            let url = {
                let mut sh = self.shared.lock().unwrap();
                match sh.archive_pending.pop_front() {
                    Some(u) => {
                        sh.archive_active.push(u.clone());
                        u
                    }
                    None => return, // queue drained — the archive is fully mirrored
                }
            };
            let mirror = mirror_path(&self.cfg.archive, &url);
            // share the per-repo clone lock with the build workers so a repo is never cloned twice
            let res = if mirror.is_dir() {
                Ok(())
            } else {
                let key = mirror.to_string_lossy().to_string();
                let lock = {
                    let mut m = self.clone_locks.lock().unwrap();
                    m.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
                };
                let _g = lock.lock().unwrap();
                if mirror.is_dir() {
                    Ok(())
                } else {
                    crate::mirror::clone_mirror(&url, &mirror, 1800, &self.stop, 3)
                }
            };
            let ts = crate::util::now();
            {
                let mut sh = self.shared.lock().unwrap();
                sh.archive_active.retain(|u| u != &url);
                let (status, reason) = match &res {
                    Ok(()) => ("added", String::new()),
                    Err(e) => ("failed", e.clone()),
                };
                sh.archive_recent.push(ArchiveRecent { repo: repo_slug(&url), status: status.into(), ts, reason });
                let len = sh.archive_recent.len();
                if len > 200 {
                    sh.archive_recent.drain(0..len - 200);
                }
            }
            self.emit("archived", &url, serde_json::json!({"status": if res.is_ok() {"added"} else {"failed"}}));
        }
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
                    // Cap concurrency at the live job limit measured at the builder-child level too:
                    // don't start new work while `jobs` builds are already actively compiling, and never
                    // start new work while ANY build is frozen by a lowered job limit — let those
                    // in-progress builds thaw and finish first (drain before new). (running is never
                    // locked while holding `sh` elsewhere, so this nested lock can't deadlock.)
                    let (unfrozen, frozen_now) = {
                        let reg = self.running.lock().unwrap();
                        (reg.unfrozen(), reg.frozen_count())
                    };
                    let ready = id < sh.jobs && !sh.paused && !sh.queue.is_empty()
                        && unfrozen < sh.jobs && frozen_now == 0;
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
                sh.packaged.remove(&slug); // a (re)build supersedes any prior package → repackage it
                slug
            };
            self.active.fetch_add(1, Ordering::Relaxed);
            // record which gflib-build override config this attempt used, so the config-watcher can tell
            // when we've since changed the fix and auto-rebuild (computed off-lock — it reads a file).
            let sig = self.config_sig(&slug);
            if let Some(r) = self.shared.lock().unwrap().results.get_mut(&slug) {
                r.config_sig = sig;
            }
            self.build_one(&slug, id);
            self.active.fetch_sub(1, Ordering::Relaxed);
            // a build just finished → refill the freed slot by thawing the oldest frozen build
            // (drain-first), and wake parked workers so they re-evaluate the gate.
            self.regulate();
            self.cond.notify_all();
            if !self.cfg.dry_run {
                self.save_state(); // never persist the mockup replay over the real session's state.json
            }
        }
    }

    fn set_result<F: FnOnce(&mut Res)>(&self, slug: &str, f: F) {
        let mut sh = self.shared.lock().unwrap();
        if let Some(r) = sh.results.get_mut(slug) {
            f(r);
        }
    }

    /// Cached exact compiler version (run once per backend/venv — the cohort python matters because
    /// different cohorts can carry different fontmake/gftools versions). `fontc_bin` is the
    /// toolchain-resolved binary (explicit / provisioned pin / detected). The cache key omits the
    /// binary path: the toolchain resolves ONCE per run, so every attempt in a run sees the same
    /// binaries — revisit if per-attempt binaries ever become a thing.
    fn compiler_version(&self, backend: &str, python: &str, fontc_bin: Option<&str>) -> String {
        let key = (backend.to_string(), python.to_string());
        {
            let sh = self.shared.lock().unwrap();
            if let Some(v) = sh.cver_cache.get(&key) {
                return v.clone();
            }
        }
        let v = compiler_version_str(backend, python, fontc_bin.or(self.cfg.fontc_bin.as_deref()));
        let mut sh = self.shared.lock().unwrap();
        sh.cver_cache.entry(key).or_insert_with(|| v.clone());
        v
    }

    /// Cached exact orchestrator version (run once per builder/venv). `builder3_bin` is the
    /// toolchain-resolved binary for builder3 probes (None is fine for builder2).
    fn builder_version(&self, builder: &str, python: &str, builder3_bin: Option<&str>) -> String {
        let key = (builder.to_string(), python.to_string());
        {
            let sh = self.shared.lock().unwrap();
            if let Some(v) = sh.bver_cache.get(&key) {
                return v.clone();
            }
        }
        let v = builder_version_str(builder, python, builder3_bin.or(self.cfg.builder3_bin.as_deref()));
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

    /// Build timings.json ({elapsed, operations, families}) — per-op timing for bottleneck analysis.
    fn timings_json(&self) -> serde_json::Value {
        let snap = self.snapshot();
        let sh = self.shared.lock().unwrap();
        let families: BTreeMap<&String, &BTreeMap<String, f64>> = sh
            .results
            .iter()
            .filter(|(_, r)| !r.timings.is_empty())
            .map(|(s, r)| (s, &r.timings))
            .collect();
        serde_json::json!({
            "elapsed": (snap.elapsed * 10.0).round() / 10.0,
            "operations": snap.op_stats,
            "families": families,
        })
    }

    /// Build the Python-format migration.json (fontc / fontmake-fallback blockers / both-agreement).
    fn migration_json(&self) -> serde_json::Value {
        let sh = self.shared.lock().unwrap();
        let built: Vec<&Res> = sh.results.values().filter(|r| r.status == "built").collect();
        let fontc: Vec<&str> = built.iter().filter(|r| r.backend == "fontc").map(|r| r.slug.as_str()).collect();
        let fm_only: Vec<&str> = built.iter().filter(|r| r.backend == "fontmake").map(|r| r.slug.as_str()).collect();
        let failed: Vec<&str> = sh.results.values().filter(|r| r.status == "failed").map(|r| r.slug.as_str()).collect();
        // the M5 (Python-free) axis: which ORCHESTRATOR built each family. builder3 = zero Python
        // in the loop; builder2+fontc = Rust compiler under Python orchestration; fontmake = all-Python.
        let b3: Vec<&str> = built.iter().filter(|r| r.builder == "builder3").map(|r| r.slug.as_str()).collect();
        let b2_fontc = built.iter().filter(|r| r.builder != "builder3" && r.backend == "fontc").count();
        serde_json::json!({
            "summary": {
                "fontc": fontc.len(),
                "fontmake_only": fm_only.len(),
                "failed": failed.len(),
                "builder3": b3.len(),
                "builder2_fontc": b2_fontc,
            },
            "fontc_built": fontc,
            "fontmake_only": fm_only,
            "builder3_built": b3,
            "failed": failed,
        })
    }

    /// Pipeline task rows for the toolchain resolver — "provision fontc/builder3" with live
    /// status, so a first run's multi-minute cargo install is visible, not a frozen dashboard.
    fn toolchain_tasks(&self) -> Vec<crate::model::TaskItem> {
        use crate::toolchain::ToolStatus;
        if self.cfg.dry_run {
            return Vec::new();
        }
        let (fc, b3) = self.toolchain.peek();
        let row = |key: &str, name: &str, st: &ToolStatus| crate::model::TaskItem {
            key: key.into(),
            name: name.into(),
            status: match st {
                ToolStatus::Pending => "running".into(),
                ToolStatus::Ready { .. } => "done".into(),
                ToolStatus::Unavailable(_) => "failed".into(),
            },
            elapsed: 0.0,
            done: matches!(st, ToolStatus::Ready { .. }) as usize,
            total: 1,
            detail: match st {
                ToolStatus::Pending => "resolving / provisioning…".into(),
                ToolStatus::Ready { path, source } => format!("{} ({})", path, source),
                ToolStatus::Unavailable(e) => e.clone(),
            },
        };
        vec![
            row("toolchain-fontc", &format!("provision fontc {}", crate::toolchain::FONTC_VERSION), &fc),
            row("toolchain-builder3", &format!("provision builder3 {}", &crate::toolchain::BUILDER3_REV[..10]), &b3),
        ]
    }

    /// Run ONE backend end-to-end into `dest` (extract → pre-build → preclean → config → build →
    /// collect). Returns (ok, err, found, bytes). Used by the --backend both path.
    #[allow(clippy::too_many_arguments)]
    fn run_backend_into(
        &self, slug: &str, fam: &Family, backend: &str, dest: &Path, work: &Path, mirror: &Path,
        python: &str, log_path: &Path, fontc_bin: Option<&str>,
    ) -> (bool, String, BTreeMap<String, PathBuf>, u64) {
        if let Err(e) = extract_tree(mirror, &fam.commit, work, EXTRACT_TIMEOUT, log_path) {
            return (false, e, BTreeMap::new(), 0);
        }
        if let Some(cmds) = self.build_rules.get(slug) {
            if let Err(e) = crate::rules::run_pre_build(work, python, cmds, log_path, self.cfg.timeout) {
                return (false, e, BTreeMap::new(), 0);
            }
        }
        preclean_outputs(work);
        let (cfg_path, _label) = match resolve_config(self.cfg.google_fonts.as_deref(), fam, work) {
            Ok(v) => v,
            Err(e) => return (false, e, BTreeMap::new(), 0),
        };
        let t0 = now();
        // the 'both' comparison runs each compiler under builder2 (the compiler axis, isolated);
        // budget applies, slice doesn't (no stable worker identity on this path)
        let total_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let inner = inner_jobs(total_cpus, self.job_limit.load(Ordering::Relaxed).max(1));
        if let Err(e) = run_builder(python, &cfg_path, work, log_path, self.cfg.timeout, "builder2",
                                    backend, fontc_bin, None, inner, None,
                                    &fam.slug, &self.running, &self.frozen, &self.job_limit) {
            return (false, format!("{}: {}", backend, e), BTreeMap::new(), 0);
        }
        let (bytes, found, extras) = collect_outputs(work, dest, &fam.shipped_fonts, t0);
        if !fam.shipped_fonts.is_empty() && found.is_empty() {
            let err = if extras.is_empty() {
                format!("{}: produced no expected font files", backend)
            } else {
                format!("{}: output name mismatch — got {:?}", backend, &extras[..extras.len().min(3)])
            };
            return (false, err, found, bytes);
        }
        (true, String::new(), found, bytes)
    }

    /// --backend both: build fontc + fontmake separately, compare, record vs/fontc_ok/fontmake_ok.
    #[allow(clippy::too_many_arguments)]
    fn build_both(
        &self, slug: &str, fam: &Family, work: &Path, out_dir: &Path, mirror: &Path, python: &str,
        builder: &str, bver: &str, log_path: &Path, fontc_bin: Option<&str>,
    ) {
        let fcver = self.compiler_version("fontc", python, fontc_bin);
        let mcver = self.compiler_version("fontmake", python, None);
        self.set_result(slug, |r| {
            r.backend = "both".into();
            r.compiler_version = format!("{}  +  {}", fcver, mcver);
            r.builder = builder.into();
            r.builder_version = bver.into();
        });
        let (fok, ferr, fbuilt, fbytes) =
            self.run_backend_into(slug, fam, "fontc", &out_dir.join("fontc"), work, mirror, python, log_path, fontc_bin);
        let (mok, merr, mbuilt, mbytes) =
            self.run_backend_into(slug, fam, "fontmake", &out_dir.join("fontmake"), work, mirror, python, log_path, None);
        if !fok && !mok {
            let msg = format!("both backends failed — fontc: {} || fontmake: {}",
                &ferr[..ferr.len().min(120)], &merr[..merr.len().min(120)]);
            let (cause, _) = crate::classify::categorize_failure(&msg);
            self.fail(slug, cause, &msg);
            return;
        }
        let vs = if fok && mok { compare_backends(&fbuilt, &mbuilt, &fam.shipped_fonts) } else { String::new() };
        let bytes = if fok { fbytes } else { mbytes };
        log_line(log_path, &format!("DONE both: fontc={} fontmake={} vs={}",
            if fok { "ok" } else { "FAIL" }, if mok { "ok" } else { "FAIL" }, if vs.is_empty() { "-" } else { &vs }));
        self.set_result(slug, |r| {
            r.status = "built".into();
            r.ended = now();
            r.out_bytes = bytes;
            r.compare = vs.clone();
            r.note = String::new();
        });
        self.emit("built", slug, serde_json::json!({"backend":"both","bytes":bytes,"vs":vs}));
        self.enqueue_qa(slug);
    }

    /// Record an operation's duration (bottleneck timing): updates the global op aggregate and the
    /// family's per-op timings (→ timings.json + the stats tab). Returns the elapsed for logging.
    fn record_op(&self, slug: &str, op: &str, dt: f64) {
        let mut sh = self.shared.lock().unwrap();
        let e = sh.op_stats.entry(op.to_string()).or_insert((0.0, 0, 0.0));
        e.0 += dt;
        e.1 += 1;
        e.2 = e.2.max(dt);
        if let Some(r) = sh.results.get_mut(slug) {
            *r.timings.entry(op.to_string()).or_insert(0.0) += dt;
        }
    }

    /// Time a closure and record it as `op` for `slug`.
    fn timed<T, F: FnOnce() -> T>(&self, slug: &str, op: &str, f: F) -> T {
        let t0 = std::time::Instant::now();
        let r = f();
        self.record_op(slug, op, t0.elapsed().as_secs_f64());
        r
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

    /// The per-family (orchestrator, compiler) attempt chain, from the live backend setting and
    /// the resolved toolchain. builder3 is ALWAYS preferred when available (it runs fontc
    /// in-process); each later pair is the graceful fallback for the one before it.
    fn attempt_pairs(&self, have_fontc: bool, have_builder3: bool) -> Vec<(&'static str, &'static str)> {
        // both the backend AND the orchestrator are live (editable via the config tab): orchestrator
        // = builder3 forces the pure-fontc path with NO builder2/fontmake fallback.
        let (backend, orchestrator) = {
            let sh = self.shared.lock().unwrap();
            (sh.backend.clone(), sh.orchestrator.clone())
        };
        attempt_chain(&backend, &orchestrator, have_fontc, have_builder3)
    }

    /// MOCKUP build: no clone/venv/compile — a brief fake "compile", then replay the family's recorded
    /// outcome so the dashboard animates a whole build with no real CPU load (for demos).
    fn dry_build_one(&self, slug: &str, worker: usize) {
        self.emit("started", slug, serde_json::json!({"worker": worker}));
        self.set_result(slug, |r| r.note = "compiling (dry-run)".into());
        // a short, deterministic fake duration per family (0.25–1.15 s) — looks live, costs nothing
        let h = slug.bytes().fold(0u64, |a, b| a.wrapping_mul(131).wrapping_add(b as u64));
        thread::sleep(Duration::from_millis(250 + h % 900));
        let pb = self.dry_playbook.get(slug).cloned();
        if matches!(&pb, Some(p) if p.status == "failed") {
            let p = pb.unwrap();
            self.set_result(slug, |r| {
                r.status = "failed".into();
                r.ended = now();
                r.error = p.error.clone();
                r.backend = p.backend.clone();
                r.compiler_version = p.compiler_version.clone();
                r.builder_version = p.builder_version.clone();
                r.note = String::new();
            });
            self.emit("failed", slug, serde_json::json!({"error": "(dry-run replay)"}));
        } else {
            let (bytes, backend, compare, cver, bver) = pb.as_ref()
                .map(|p| (p.out_bytes, p.backend.clone(), p.compare.clone(), p.compiler_version.clone(), p.builder_version.clone()))
                .unwrap_or((123_456, "fontc".into(), "identical".into(), String::new(), String::new()));
            self.set_result(slug, |r| {
                r.status = "built".into();
                r.ended = now();
                r.out_bytes = bytes;
                r.backend = if backend.is_empty() { "fontc".into() } else { backend };
                r.compare = compare;
                r.compiler_version = cver;
                r.builder_version = bver;
                r.note = String::new();
            });
            self.emit("built", slug, serde_json::json!({"backend": "dry-run", "bytes": bytes}));
        }
    }

    /// Build one family end-to-end. Mirrors the Python `_build` flow (single-backend / auto path).
    fn build_one(&self, slug: &str, _worker: usize) {
        if self.cfg.dry_run {
            self.dry_build_one(slug, _worker);
            return;
        }
        let fam = {
            let sh = self.shared.lock().unwrap();
            match sh.families.get(slug) {
                Some(f) => f.clone(),
                None => return,
            }
        };
        // auto-UPGRADE runs keep a full snapshot of the prior (built) result: if no better rung
        // succeeds, the snapshot is restored verbatim — an upgrade can never cost a success.
        let upgrade_prior: Option<Res> = {
            let sh = self.shared.lock().unwrap();
            sh.results.get(slug).filter(|r| r.queued_kind == "upgrade").cloned()
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
            self.set_result(slug, |r| r.note = "resolving deps".into()); // checking for a cached cohort venv
            // in multi-Python mode, the commit year picks the starting ladder rung (skip too-new interpreters)
            let cyear = if self.cfg.pythons.len() > 1 { commit_year(&mirror, &fam.commit) } else { None };
            // name what we'd install, so the status says WHICH deps (the on_install cb fires only when a
            // venv is actually built, not when a cached one is reused — so it never lies about installing)
            let dep_names = v.dep_names(&req);
            let (py, cohort, pyver, verr) = self.timed(slug, "venv", || v.get_python(&req, cyear, |_k| {
                let n = dep_names.len();
                let shown = dep_names.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                // NOTE: must keep the "installing deps" prefix — both UIs match it to flag an install that
                // will freeze on reaching the compile step (web buildingRow / TUI 'INS→').
                let note = match n {
                    0 => "installing deps".to_string(),
                    1..=4 => format!("installing deps ({}): {}", n, shown),
                    _ => format!("installing deps ({}): {}, …", n, shown),
                };
                self.set_result(slug, |r| r.note = note);
            }));
            if !verr.is_empty() {
                // The build never reached the per-family build step, so logs/<fam>.log is empty
                // ("(no log yet)"). The real story is in the COHORT install log — copy it into this
                // family's log so the detail view's log-tail shows what actually failed at pip-install.
                let install_log = self.cfg.build_dir.join("venvs").join(format!("{}.install.log", cohort));
                if let Ok(content) = std::fs::read_to_string(&install_log) {
                    let _ = std::fs::write(&log_path, format!(
                        "(venv install failed — the per-family build never ran. Below is the cohort install \
                         log: venvs/{}.install.log)\n\n{}",
                        cohort, content));
                }
                let msg = format!("venv: {}", verr);
                let (cause, _) = crate::classify::categorize_failure(&msg);
                self.fail(slug, cause, &msg);
                cleanup(&work, self.cfg.keep_work);
                return;
            }
            if !pyver.is_empty() {
                self.set_result(slug, |r| r.python_version = pyver.clone());
            }
            self.note_cohort(slug, &cohort, &req);
            // The cohort venv is now installed+marked on disk. Reflect it in the cohorts view
            // immediately instead of waiting for the 10s size-thread rescan of cached_cohorts — else a
            // fast first build can turn its family green while the cohort dot is still grey (the
            // transient the screenshot caught). The next rescan re-confirms it; insert is idempotent.
            if !cohort.is_empty() {
                self.shared.lock().unwrap().cached_cohorts.insert(cohort.clone());
            }
            self.set_result(slug, |r| {
                r.cohort = cohort.clone();
                r.note = String::new();
            });
            py
        } else {
            self.cfg.build_python.clone()
        };

        // Wait for the toolchain verdicts (fontc + builder3). On a first run this may be the
        // auto-provisioner cargo-installing the pins — minutes, visible in the pipeline tasks —
        // so tell the UI what this family is blocked on. Subsequent runs return instantly.
        self.set_result(slug, |r| r.note = "waiting for toolchain".into());
        let (fontc_bin, builder3_bin) = self.toolchain.wait();
        self.set_result(slug, |r| r.note = String::new());

        // --backend both (fontc_crater-style): build with BOTH compilers into separate dirs and
        // compare their outputs — under builder2 on both sides, so the comparison isolates the
        // COMPILER axis (builder3-vs-builder2 equivalence is a separate, future comparison).
        if self.shared.lock().unwrap().backend == "both" {
            let bver = self.builder_version("builder2", &python, None);
            self.build_both(slug, &fam, &work, &out_dir, &mirror, &python, "builder2", &bver, &log_path, fontc_bin.as_deref());
            cleanup(&work, self.cfg.keep_work);
            return;
        }

        // the stamp records what was actually REACHABLE for this attempt (pins + availability),
        // so a result produced during a degraded run re-arms once the missing tool provisions. Use the
        // LIVE orchestrator (what attempt_pairs actually uses) so the stamp matches the real attempt chain
        // after a live orchestrator change — not the stale launch cfg.
        let live_orchestrator = self.shared.lock().unwrap().orchestrator.clone();
        let tc_sig = crate::toolchain::run_sig(
            &live_orchestrator, fontc_bin.is_some(), builder3_bin.is_some());
        let mut pairs = self.attempt_pairs(fontc_bin.is_some(), builder3_bin.is_some());
        let mut stash: Option<PathBuf> = None;
        if let Some(prior) = &upgrade_prior {
            // already attempted under EXACTLY these capabilities? nothing new to learn — quick
            // no-op restore (this is the common per-run path while a tool stays unavailable)
            if prior.upgrade_attempted == tc_sig {
                self.restore_prior(slug, prior, &tc_sig, "already attempted under this toolchain");
                cleanup(&work, self.cfg.keep_work);
                return;
            }
            // an upgrade only ever tries rungs STRICTLY better than the kept success
            let floor = result_rung(&prior.builder, &prior.backend);
            pairs.retain(|(b, c)| result_rung(b, c) > floor);
            if pairs.is_empty() {
                // nothing better is reachable right now (e.g. builder3 unavailable): keep the
                // success untouched and stamp the signature so this isn't re-tried until a pin bump
                self.restore_prior(slug, prior, &tc_sig, "no better rung available");
                cleanup(&work, self.cfg.keep_work);
                return;
            }
            // park the prior rung's binaries: kept for comparison on success, restored on decline
            stash = stash_variant_outputs(&self.cfg.build_dir, &out_dir, &logname, prior);
        }
        if pairs.is_empty() {
            // report the LIVE backend+orchestrator (what attempt_pairs actually used), not the launch cfg —
            // e.g. a live switch to orchestrator=builder3 with builder3 unavailable lands here.
            let (backend, orchestrator) = {
                let sh = self.shared.lock().unwrap();
                (sh.backend.clone(), sh.orchestrator.clone())
            };
            self.fail(slug, "no usable backend", &format!(
                "no orchestrator/compiler can run --backend {} (orchestrator={}, fontc {}, builder3 {})",
                backend, orchestrator,
                if fontc_bin.is_some() { "ok" } else { "unavailable" },
                if builder3_bin.is_some() { "ok" } else { "unavailable" },
            ));
            cleanup(&work, self.cfg.keep_work);
            return;
        }

        let mut last_err = String::new();
        let mut fontc_err = String::new();
        let mut built_any = false;

        for (i, (builder, backend)) in pairs.iter().enumerate() {
            let builder: &str = builder;
            let backend: &str = backend;
            // "delete everything" set the abort flag: a SIGKILLed compile returns Err and we land back
            // here — do NOT start another attempt (that's why a one-shot kill alone kept families
            // "building"). Bail the whole family so the worker returns and the drain can complete.
            if self.abort_builds.load(Ordering::Relaxed) {
                last_err = "aborted (delete everything)".into();
                break;
            }
            // fresh extraction for each backend attempt (a previous attempt may have dirtied work/)
            self.set_result(slug, |r| r.note = "checkout".into());
            if let Err(e) = self.timed(slug, "extract", || extract_tree(&mirror, &fam.commit, &work, EXTRACT_TIMEOUT, &log_path)) {
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
            let (cfg_path, label) = match self.timed(slug, "config", || resolve_config(self.cfg.google_fonts.as_deref(), &fam, &work)) {
                Ok(v) => v,
                Err(e) => {
                    last_err = e;
                    break; // no config -> trying another backend won't help
                }
            };
            let cver = self.compiler_version(backend, &python, fontc_bin.as_deref());
            let bver = self.builder_version(builder, &python, builder3_bin.as_deref());
            self.set_result(slug, |r| {
                r.backend = backend.to_string();
                r.compiler_version = cver.clone();
                r.builder = builder.to_string();
                r.builder_version = bver.clone();
                r.note = String::new();
            });
            log_line(&log_path, &format!(
                "build[{}/{}]: {} {} via {} · config={} — running {}…",
                builder, backend, backend, cver, bver, label, builder
            ));
            let t0 = now();
            // per-build CPU budget from the LIVE jobs target (recomputed per attempt, so a live
            // jobs change reshapes the budget for subsequent builds)
            let total_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            let inner = inner_jobs(total_cpus, self.job_limit.load(Ordering::Relaxed).max(1));
            let slice = if self.cfg.cpu_slices {
                Some(cpu_slice(_worker, inner, total_cpus))
            } else {
                None
            };
            // an abort may have landed during checkout/venv/config — don't start a fresh compile
            if self.abort_builds.load(Ordering::Relaxed) {
                last_err = "aborted (delete everything)".into();
                break;
            }
            let run = self.timed(slug, "build", || run_builder(
                &python,
                &cfg_path,
                &work,
                &log_path,
                self.cfg.timeout,
                builder,
                backend,
                fontc_bin.as_deref(),
                builder3_bin.as_deref(),
                inner,
                slice.as_deref(),
                slug,
                &self.running,
                &self.frozen,
                &self.job_limit,
            ));
            if let Err(e) = run {
                // prefix failures by the attempt that produced them: "builder3: …" identifies an
                // orchestrator-level failure; builder2 attempts keep the compiler-name prefix the
                // failure classifier already understands.
                last_err = if builder == "builder3" {
                    format!("builder3: {}", e)
                } else {
                    format!("{}: {}", backend, e)
                };
                if backend == "fontc" {
                    fontc_err = last_err.clone();
                }
                continue;
            }
            // collect only fonts written during THIS build (mtime gate), recursively
            let (bytes, found, extras) = self.timed(slug, "collect", || collect_outputs(&work, &out_dir, &fam.shipped_fonts, t0));
            if !fam.shipped_fonts.is_empty() && found.is_empty() {
                let who = if builder == "builder3" { "builder3" } else { backend };
                last_err = if extras.is_empty() {
                    format!("{}: produced no expected font files", who)
                } else {
                    format!("{}: output name mismatch — got {:?}", who, &extras[..extras.len().min(3)])
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
            let used = backend.to_string();
            let sig = tc_sig.clone();
            self.set_result(slug, |r| {
                r.status = "built".into();
                r.ended = now();
                r.out_bytes = bytes;
                r.out_missing = missing;
                r.backend = used.clone();
                r.compare = cmp.clone();
                r.note = String::new();
                r.queued_kind = String::new();
                // this run's chain already tried every better rung first — stamp the signature so
                // the auto-upgrade pass doesn't pointlessly re-attempt under the same toolchain
                r.upgrade_attempted = sig;
            });
            log_line(&log_path, &format!(
                "DONE: builder={} backend={} bytes={} missing={}", builder, backend, bytes, missing
            ));
            if let (Some(prior), Some(v)) = (&upgrade_prior, &stash) {
                // both rungs' binaries are now on disk: the new canonical in out/, the superseded
                // rung under variants/ — kept so the outputs can be compared later (M3)
                log_line(&log_path, &format!(
                    "UPGRADED from {} via {} — its binaries are kept for comparison at {}",
                    prior.backend, prior.builder, v.display()
                ));
                self.shared.lock().unwrap().control_log.push(format!(
                    "upgrade {}: {} → {} (prior binaries kept under variants/)", slug, prior.backend, used
                ));
            }
            self.emit("built", slug, serde_json::json!({"backend": used, "builder": builder, "bytes": bytes, "compare": cmp}));
            self.enqueue_qa(slug);
            // keep the built fonts when deb-packaging is on: the live package worker needs them on
            // disk (otherwise --discard-fonts would prune them before it can assemble the .deb).
            if !self.cfg.keep_fonts && !self.shared.lock().unwrap().build_debs {
                let _ = std::fs::remove_dir_all(&out_dir);
            }
            let _ = i;
            break;
        }

        if !built_any {
            let err = if last_err.is_empty() { "build failed".into() } else { last_err };
            if let Some(prior) = &upgrade_prior {
                // the upgrade didn't pan out — the kept success is RESTORED, never overwritten
                // (result fields AND the parked binaries). The attempt details stay in the family
                // log; the failure tabs/history are for real regressions, not declined upgrades.
                if let Some(v) = &stash {
                    unstash_outputs(v, &out_dir);
                }
                log_line(&log_path, &format!("UPGRADE declined (prior {} via {} kept): {}", prior.backend, prior.builder, err));
                self.emit("upgrade_declined", slug, serde_json::json!({"kept": prior.backend, "error": err}));
                self.restore_prior(slug, prior, &tc_sig, &err);
                cleanup(&work, self.cfg.keep_work);
                return;
            }
            let (cause, _) = crate::classify::categorize_failure(&err);
            self.fail(slug, cause, &err);
            let _ = fontc_err;
        }
        cleanup(&work, self.cfg.keep_work);
    }

    /// Put a kept success back after a declined auto-upgrade: the prior result verbatim (backend,
    /// builder, versions, bytes, compare, ended), status `built`, stamped with the toolchain
    /// signature so the upgrade isn't re-attempted until a pin/preference change.
    fn restore_prior(&self, slug: &str, prior: &Res, sig: &str, why: &str) {
        let p = prior.clone();
        let sig = sig.to_string();
        self.set_result(slug, |r| {
            *r = p;
            r.status = "built".into();
            r.note = String::new();
            r.queued_kind = String::new();
            r.upgrade_attempted = sig;
        });
        self.shared.lock().unwrap().control_log.push(format!(
            "upgrade {}: kept existing result ({})", slug, &why[..why.len().min(120)]
        ));
        self.cond.notify_all();
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
            // Seed from the CURRENT on-disk seq so a pre-existing control.json is NOT replayed at startup.
            // control.json is never cleared and seq grows monotonically; a startup replay would re-run the
            // last one-shot control (e.g. a reset_portion) and clobber freshly-resolved CLI/config settings.
            // Live state already survives a UI restart via live_overrides_argv, and durably via state.json /
            // gflib-build.config — nothing depends on replaying the last control. Only act on FUTURE bumps.
            let mut last = persist::read_control(&me.cfg.build_dir).map(|c| c.seq).unwrap_or(0);
            while !me.stop.load(Ordering::Relaxed) {
                if let Some(ctl) = persist::read_control(&me.cfg.build_dir) {
                    if ctl.seq != last {
                        last = ctl.seq;
                        me.apply_live(&ctl.set);
                        // reset-tab deletions run off-thread (a venv tree can be many GB); handled
                        // here rather than in apply_live because spawning needs the Arc
                        if let Some(key) = ctl.set.reset_portion.clone() {
                            me.spawn_reset_portion(key);
                        }
                    }
                }
                // Poll control.json briskly so live controls (jobs/pause/backend, reset-tab deletes)
                // apply with no perceptible lag — it's a tiny file, only acted on when its seq changes.
                thread::sleep(Duration::from_millis(200));
            }
        });
    }

    /// The reset tab's deletable portions with live sizes. Everything is under the build dir
    /// (plus the provisioned toolchain under data-dir); the repo archive and google/fonts clone
    /// are NEVER part of any portion — the bare git archive can never be deleted from here.
    /// The font-binary portions CASCADE: deleting a family's fonts also deletes its build log and
    /// resets its result to queued, so the progress bar regresses (see spawn_reset_portion). The
    /// other portions keep build results untouched.
    fn compute_reset_portions(&self) -> Vec<crate::model::ResetPortion> {
        use crate::model::ResetPortion;
        let bd = &self.cfg.build_dir;
        // (logname, backend) for the families each font portion resets: BUILT (binaries to delete) and
        // FAILED (re-queued + their logs deleted, so the bar's red segment regresses too).
        let (by_backend, failed_by_backend): (Vec<(String, String)>, Vec<(String, String)>) = {
            let sh = self.shared.lock().unwrap();
            let built = sh.results.values().filter(|r| r.status == "built")
                .map(|r| (slug_to_logname(&r.slug), r.backend.clone())).collect();
            let failed = sh.results.values().filter(|r| r.status == "failed")
                .map(|r| (slug_to_logname(&r.slug), r.backend.clone())).collect();
            (built, failed)
        };
        let out_root = bd.join("out");
        let logs_root = bd.join("logs");
        // a family's logs = the live log AND its archived failure copy (logs/failed/) — the cascade deletes both
        let log_bytes = |logname: &str| {
            let live = std::fs::metadata(logs_root.join(format!("{}.log", logname))).map(|m| m.len()).unwrap_or(0);
            let arch = std::fs::metadata(logs_root.join("failed").join(format!("{}.log", logname))).map(|m| m.len()).unwrap_or(0);
            live + arch
        };
        // Per-compiler footprint = the out/ binaries PLUS the per-family build logs the cascade would
        // delete. `reset_any` tracks whether the portion would re-queue at least one family — so the
        // button stays actionable when only logs (or only the 'built' result) remain after the binaries
        // were already removed, instead of going dead at bytes==0 (an otherwise unreachable state).
        let mut fontc_bytes = 0u64;
        let mut fontmake_bytes = 0u64;
        let mut fontc_reset_any = false;
        let mut fontmake_reset_any = false;
        for (logname, backend) in &by_backend {
            match backend.as_str() {
                "fontc" => { // a fontc-built family is always fully un-built by this portion
                    fontc_bytes += dir_size(&out_root.join(logname)) + log_bytes(logname);
                    fontc_reset_any = true;
                }
                "fontmake" => {
                    fontmake_bytes += dir_size(&out_root.join(logname)) + log_bytes(logname);
                    fontmake_reset_any = true;
                }
                "both" => {
                    let fc = dir_size(&out_root.join(logname).join("fontc"));
                    let fm = dir_size(&out_root.join(logname).join("fontmake"));
                    fontc_bytes += fc;
                    fontmake_bytes += fm;
                    // a 'both' family is reset (and its log deleted) by one compiler's portion only once
                    // the OTHER compiler's half is also gone — match families_to_requeue's rule.
                    if fm == 0 { fontc_bytes += log_bytes(logname); fontc_reset_any = true; }
                    if fc == 0 { fontmake_bytes += log_bytes(logname); fontmake_reset_any = true; }
                }
                _ => {}
            }
        }
        // FAILED families are ALSO reset by the matching font button (re-queued + logs deleted), so the
        // red "failed" bar segment regresses too. A backend-less failure (no compiler chosen yet) is
        // reachable from EITHER button. Count their logs so the button is sized and stays actionable when
        // only failures remain (no binaries, no built results).
        for (logname, backend) in &failed_by_backend {
            let lb = log_bytes(logname);
            let b = backend.as_str();
            if b == "fontc" || b == "both" || b.is_empty() { fontc_bytes += lb; fontc_reset_any = true; }
            if b == "fontmake" || b == "both" || b.is_empty() { fontmake_bytes += lb; fontmake_reset_any = true; }
        }
        let p = |key: &str, label: &str, bytes: u64, hint: &str| ResetPortion {
            key: key.into(), label: label.into(), bytes, hint: hint.into(),
            actionable: bytes > 0, ..Default::default() // most portions: clickable iff there's something on disk
        };
        vec![
            // the global nuke: stop jobs, pause, wipe the whole build dir + toolchain (NOT the archive)
            ResetPortion { actionable: true,
              ..p("all", "⚠ DELETE EVERYTHING — stop jobs · pause · wipe all build data",
                  dir_size(bd).saturating_add(dir_size(&self.cfg.data_dir.join("tools"))),
                  "stops every running job, pauses the build, then permanently deletes ALL build data — outputs, logs, venvs, packages, state, the provisioned toolchain — everything under the build dir plus data-dir/tools, EXCEPT the bare git repo archive (and the google/fonts clone). Every family resets to queued; the build stays PAUSED so you can resume to rebuild from scratch") },
            ResetPortion { actionable: fontc_bytes > 0 || fontc_reset_any,
              ..p("fonts-fontc", "font binaries built by fontc", fontc_bytes,
              "deletes fontc-built fonts (incl. builder3) + the logs of fontc-built AND fontc-failed families, re-queuing them all — both the built and the red failed bar segments regress (pause first if you don't want them rebuilt)") },
            ResetPortion { actionable: fontmake_bytes > 0 || fontmake_reset_any,
              ..p("fonts-fontmake", "font binaries built by fontmake", fontmake_bytes,
              "deletes fontmake-built fonts + the logs of fontmake-built AND fontmake-failed families, re-queuing them all — both the built and the red failed bar segments regress (pause first if you don't want them rebuilt)") },
            p("variants", "superseded upgrade binaries (variants/)", dir_size(&bd.join("variants")),
              "older rungs' binaries kept by successful auto-upgrades, for output comparison"),
            p("debs", ".deb packages + packaging trees", dir_size(&bd.join("packaging")),
              "deletes the whole packaging/ tree; the package worker re-drafts from the existing fonts"),
            p("venvs", "dependency-cohort venvs", dir_size(&bd.join("venvs")),
              "cohort venvs are recreated on demand from the pinned requirements (first builds get slower)"),
            p("pip-cache", "pip download cache", dir_size(&bd.join("pip-cache")),
              "wheels/sdists re-download on the next venv creation"),
            p("logs", "per-family build logs", dir_size(&bd.join("logs")),
              "includes the archived failure logs; failure-history.jsonl is kept"),
            p("work", "leftover work extractions", dir_size(&bd.join("work")),
              "throwaway trees (normally cleaned after each build; --keep-work leaves them)"),
            p("fontspector", "fontspector QA results", dir_size(&bd.join("fontspector")),
              "the QA pass recreates them on the next run"),
            p("tools", "provisioned toolchain (fontc/builder3)", dir_size(&self.cfg.data_dir.join("tools")),
              "pinned binaries re-provision automatically on the next run (minutes)"),
        ]
    }

    /// Execute one reset-tab deletion off-thread. Instead of refusing the whole portion when any
    /// build is in flight, it deletes every ITEM that is not actively in use and SKIPS the few
    /// that are: per-family artifacts of currently-building families are kept, as are cohort venvs
    /// a building family is using and (while any build runs) the shared toolchain it executes.
    fn spawn_reset_portion(self: &Arc<Self>, key: String) {
        let me = Arc::clone(self);
        thread::spawn(move || {
            me.shared.lock().unwrap().reset_notes.remove(&key); // fresh attempt → clear any old outcome
            if key == "all" {
                // the global "Delete everything!" button — its own stop→pause→wipe sequence
                if me.cfg.dry_run {
                    let now = me.elapsed();
                    me.shared.lock().unwrap().reset_notes
                        .insert("all".into(), ("⛔ disabled in --dry-run (the mockup never wipes the real build dir)".into(), now));
                    return;
                }
                // re-entrancy guard: a double-click (or a stale control replay) must not start a second
                // concurrent wipe racing the first. Held for the whole reset_everything run.
                if me.reset_all_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                    return; // a delete-everything is already in progress — ignore the duplicate
                }
                me.reset_everything();
                me.reset_all_running.store(false, Ordering::SeqCst);
                return;
            }
            let bd = me.cfg.build_dir.clone();

            // what's actively in use right now — never deleted out from under a running build.
            // `built_trips` is (slug, logname, backend) for every built family, captured up front:
            // select_font_targets only needs (logname, backend), but the cascade below also needs the
            // slug to reset that family's result so the progress bar regresses.
            let (building_lognames, in_use_cohorts, n_building, built_trips) = {
                let sh = me.shared.lock().unwrap();
                let building: HashSet<String> = sh.results.values()
                    .filter(|r| r.status == "building").map(|r| slug_to_logname(&r.slug)).collect();
                let cohorts: HashSet<String> = sh.results.values()
                    .filter(|r| r.status == "building" && !r.cohort.is_empty())
                    .map(|r| r.cohort.clone()).collect();
                let trips: Vec<(String, String, String)> = sh.results.values().filter(|r| r.status == "built")
                    .map(|r| (r.slug.clone(), slug_to_logname(&r.slug), r.backend.clone())).collect();
                (building, cohorts, sh.results.values().filter(|r| r.status == "building").count(), trips)
            };

            // gather the concrete paths to delete for this portion, already excluding in-use items
            let mut targets: Vec<PathBuf> = Vec::new();
            let mut skipped = 0usize;
            // immediate children of `dir` keyed by a family logname (file stem or dir name): keep
            // the ones currently building, delete the rest. `strip` removes a suffix like ".log".
            let mut per_family_dir = |dir: &Path, strip: &str| {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for e in entries.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        let logname = name.strip_suffix(strip).unwrap_or(&name).to_string();
                        if building_lognames.contains(&logname) { skipped += 1; } else { targets.push(e.path()); }
                    }
                }
            };
            match key.as_str() {
                "fonts-fontc" | "fonts-fontmake" => {
                    let compiler = key.trim_start_matches("fonts-");
                    let lb: Vec<(String, String)> = built_trips.iter()
                        .map(|(_, logname, backend)| (logname.clone(), backend.clone())).collect();
                    let (t, s) = select_font_targets(&bd.join("out"), &lb, compiler, &building_lognames);
                    targets = t; skipped += s;
                }
                "variants" => per_family_dir(&bd.join("variants"), ""),
                "work" => per_family_dir(&bd.join("work"), ""),
                "fontspector" => per_family_dir(&bd.join("fontspector"), ""),
                "logs" => {
                    per_family_dir(&bd.join("logs"), ".log"); // keep a building family's live log
                    // logs/failed/ archived copies aren't a building family's live log → fair game
                    // (per_family_dir already queued it as a dir entry named "failed"; that's fine)
                }
                "debs" => {
                    // per-family debian trees keep building families'; the global pool/index go
                    let pkg = bd.join("packaging");
                    if let Ok(entries) = std::fs::read_dir(&pkg) {
                        for e in entries.flatten() {
                            let name = e.file_name().to_string_lossy().to_string();
                            if building_lognames.contains(&name) { skipped += 1; } else { targets.push(e.path()); }
                        }
                    }
                }
                "venvs" => {
                    if let Ok(entries) = std::fs::read_dir(bd.join("venvs")) {
                        for e in entries.flatten() {
                            let cohort = e.file_name().to_string_lossy().to_string();
                            // a venv is in use if a building family's cohort key is a prefix of it
                            // (the multi-Python ladder appends -py<tag> to the base cohort key)
                            let in_use = in_use_cohorts.iter().any(|c| cohort == *c || cohort.starts_with(&format!("{}-py", c)));
                            if in_use { skipped += 1; } else { targets.push(e.path()); }
                        }
                    }
                }
                "pip-cache" => targets.push(bd.join("pip-cache")), // shared; worst case a re-download
                "tools" => {
                    // the provisioned binaries are executed by EVERY in-flight build; deleting them
                    // would break not-yet-started attempts. Allowed only when nothing is building.
                    if n_building > 0 {
                        let now = me.elapsed();
                        let msg = format!("⛔ kept — in use by {} running build(s); pause to delete", n_building);
                        let mut sh = me.shared.lock().unwrap();
                        sh.control_log.push(format!("reset tools: {}", msg));
                        sh.reset_notes.insert(key.clone(), (msg, now));
                        return;
                    }
                    targets.push(me.cfg.data_dir.join("tools"));
                }
                other => {
                    me.shared.lock().unwrap().control_log.push(format!("reset: unknown portion '{}'", other));
                    return;
                }
            }

            // measure the selected targets up front so the bar has a stable denominator
            let total: u64 = targets.iter().map(|p| dir_size(p)).sum();
            me.shared.lock().unwrap().reset_progress.insert(key.clone(), (0, total));

            // publish freed-bytes at most ~5×/s — the status writer snapshots every ~1 s anyway
            let mut last_pub = std::time::Instant::now();
            let me2 = Arc::clone(&me);
            let k2 = key.clone();
            let mut publish = move |freed: u64| {
                if last_pub.elapsed() >= Duration::from_millis(200) {
                    last_pub = std::time::Instant::now();
                    me2.shared.lock().unwrap().reset_progress.insert(k2.clone(), (freed, total));
                }
            };
            let mut freed = 0u64;
            for p in &targets {
                if p.is_dir() { remove_tree_progress(p, &mut freed, &mut publish); }
                else if let Ok(m) = std::fs::metadata(p) {
                    freed += m.len();
                    let _ = std::fs::remove_file(p);
                    publish(freed);
                }
            }

            let mut requeued = 0usize;
            match key.as_str() {
                "debs" => me.shared.lock().unwrap().packaged.clear(),
                "venvs" => me.shared.lock().unwrap().cached_cohorts.retain(|c| in_use_cohorts.contains(c)),
                "logs" => { let _ = std::fs::create_dir_all(bd.join("logs")); } // every build needs logs/
                // In --dry-run the control watcher is live and the mockup marks families "built", so a
                // reset click here would otherwise re-queue mockup results and (via save_state) clobber the
                // real session's state.json — every persistence site in this file is dry-run-guarded for
                // exactly that reason. Skip the whole cascade in dry-run; it falls through to the no-op arm.
                "fonts-fontc" | "fonts-fontmake" if !me.cfg.dry_run => {
                    // Cascade: reset this compiler's families to queued and delete their logs, so the bar
                    // regresses. Two groups: (1) BUILT families this deletion left with NO fonts (built↓,
                    // queued↑); (2) FAILED families of this backend (failed↓, queued↑) — their logs are the
                    // only thing the binary delete couldn't touch (a failed build has no fonts). The
                    // denominator (built+failed+queued+building) is preserved, so the bar visibly REGRESSES
                    // rather than families silently dropping off the worklist. We also clear the packaged
                    // marker (so a rebuild re-packages). Re-queued families rebuild on the running pool
                    // unless the daemon is paused. A 'both'-backend BUILT family is reset only once BOTH
                    // compiler halves are gone — this click may have removed only one. The bare git archive
                    // is never a target, so it can't be touched.
                    let compiler = key.trim_start_matches("fonts-");
                    let other = if compiler == "fontc" { "fontmake" } else { "fontc" };
                    let out_root = bd.join("out");
                    let logs_dir = bd.join("logs");
                    let failed_dir = logs_dir.join("failed");
                    // BUILT families left with NO fonts by this deletion (checked AFTER it ran) ...
                    let mut victims = families_to_requeue(&built_trips, compiler, &building_lognames,
                        |logname| dir_size(&out_root.join(logname).join(other)) > 0);
                    let mut sh = me.shared.lock().unwrap();
                    // ... PLUS every FAILED family of this backend — and backend-less failures, reachable from
                    // either button. Re-queue them and delete their logs too, so the red "failed" segment
                    // regresses as well (a failed build has no binaries, so the binary delete above skips it).
                    for r in sh.results.values() {
                        if r.status == "failed" {
                            let b = r.backend.as_str();
                            if b == compiler || b == "both" || b.is_empty() {
                                victims.push((r.slug.clone(), slug_to_logname(&r.slug)));
                            }
                        }
                    }
                    for (slug, logname) in &victims {
                        {
                            // act only on a still-live built/failed result (a build could have raced in)
                            let Some(r) = sh.results.get_mut(slug) else { continue };
                            if r.status != "built" && r.status != "failed" { continue; }
                            r.status = "queued".into();
                            r.queued_kind = "rebuild".into();
                            r.error.clear();
                        }
                        requeued += 1;
                        sh.packaged.remove(slug);
                        if !sh.queue.contains(slug) { sh.queue.push_back(slug.clone()); }
                        // delete the live log AND its archived failure copy, counting both toward freed
                        for lp in [logs_dir.join(format!("{}.log", logname)), failed_dir.join(format!("{}.log", logname))] {
                            if let Ok(m) = std::fs::metadata(&lp) { freed += m.len(); }
                            let _ = std::fs::remove_file(&lp);
                        }
                    }
                    drop(sh);
                    if requeued > 0 {
                        me.save_state();      // persist the regression so a restart agrees
                        me.cond.notify_all(); // wake the pool to rebuild (a no-op while paused)
                    }
                }
                _ => {}
            }

            let portions = me.compute_reset_portions();
            let now = me.elapsed();
            let mut sh = me.shared.lock().unwrap();
            sh.reset_progress.remove(&key);
            sh.reset_portions = portions;
            let mut msg = if skipped > 0 {
                format!("✓ freed {} (kept {} in use)", crate::util::human(freed), skipped)
            } else {
                format!("✓ freed {}", crate::util::human(freed))
            };
            if requeued > 0 {
                msg.push_str(&format!(" · re-queued {} famil{} (progress regressed)",
                    requeued, if requeued == 1 { "y" } else { "ies" }));
            }
            sh.reset_notes.insert(key.clone(), (msg.clone(), now)); // lingers ~6 s
            sh.control_log.push(format!("reset {}: {}", key, msg));
        });
    }

    /// The global "Delete everything!" button (reset-tab key "all"): STOP every running job, PAUSE so the
    /// pool starts nothing new, then WIPE all build data — outputs, logs, venvs, packages, state and the
    /// provisioned toolchain (everything under the build dir except the daemon's own pid/control files,
    /// plus data-dir/tools). The bare git repo archive and the google/fonts clone are NEVER touched. Every
    /// family is reset to queued and the build stays PAUSED, so the user gets a clean slate to inspect and
    /// can resume to rebuild from scratch. Runs in the reset thread (off the control watcher).
    fn reset_everything(self: &Arc<Self>) {
        let bd = self.cfg.build_dir.clone();
        let tools = self.cfg.data_dir.join("tools");
        // SAFETY GUARD: never wipe if the bare git archive (or the google/fonts clone) lives INSIDE the
        // build dir — the one thing "delete everything" must never destroy. Mirrors the CLI run_reset guard.
        // Refuse, leave everything intact (don't even pause).
        {
            let bdc = bd.canonicalize().unwrap_or_else(|_| bd.clone());
            let inside = |p: &std::path::Path| {
                p.canonicalize().map(|pc| pc == bdc || pc.starts_with(&bdc)).unwrap_or(false)
            };
            let blocked = if inside(&self.cfg.archive) {
                Some("bare git repo archive")
            } else if self.cfg.google_fonts.as_deref().map(|g| inside(g)).unwrap_or(false) {
                Some("google/fonts clone")
            } else {
                None
            };
            if let Some(what) = blocked {
                let now = self.elapsed();
                let mut sh = self.shared.lock().unwrap();
                let msg = format!("⛔ refused — the {} lives inside the build dir; move it first", what);
                sh.reset_notes.insert("all".into(), (msg.clone(), now));
                sh.control_log.push(format!("delete everything: {}", msg));
                return;
            }
        }
        // 1) pause: the worker pool starts nothing new while paused (worker_loop's ready-gate checks it)
        {
            let mut sh = self.shared.lock().unwrap();
            sh.paused = true;
            sh.reset_notes.insert("all".into(), ("stopping all running jobs…".into(), self.elapsed()));
            sh.control_log.push("delete everything: pausing and stopping all running jobs…".into());
        }
        self.frozen.store(true, Ordering::Relaxed);
        // ABORT in-flight builds: build_one bails between attempts (so a killed compile doesn't just
        // advance to the next backend) and the venv installer kills its pip tree. Without this a one-shot
        // SIGKILL only stopped the current attempt; the family then restarted on the next (builder,backend).
        self.abort_builds.store(true, Ordering::Relaxed);
        // 2) terminate every in-flight builder process group (thaw a frozen child first so it can die)
        self.signal_running(SIGCONT);
        self.signal_running(SIGKILL);
        self.cond.notify_all();
        // 3) DRAIN every worker pool that writes into the build dir — build (active / "building"), QA
        //    (qa_active) and packaging (pkg_now). Paused/frozen ⇒ each parks at its next loop top and takes
        //    no new work, so once these clear they stay clear. Bounded by a 60s deadline.
        let busy = || {
            self.active.load(Ordering::Relaxed) > 0 || {
                let sh = self.shared.lock().unwrap();
                sh.results.values().any(|r| r.status == "building")
                    || !sh.qa_active.is_empty()
                    || !sh.pkg_now.is_empty()
            }
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while busy() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        // If a job couldn't be stopped in time — a venv install / git clone in the build prologue isn't a
        // registered process group, so SIGKILL can't reach it — REFUSE the wipe rather than delete out from
        // under a live build (the CLI run_reset refuses-when-running for the same reason). Stay PAUSED so
        // the user can retry once it settles; nothing was deleted and no results were reset.
        if busy() {
            self.abort_builds.store(false, Ordering::Relaxed); // didn't wipe — let the in-flight builds finish normally
            let now = self.elapsed();
            let mut sh = self.shared.lock().unwrap();
            let msg = "⛔ couldn't stop all jobs within 60s (a clone or pre-build step is in flight) — nothing deleted; still PAUSED, click again once it settles".to_string();
            sh.reset_notes.insert("all".into(), (msg.clone(), now));
            sh.control_log.push(format!("delete everything: {}", msg));
            return;
        }
        // drained — clear the abort flag so the rebuild after resume proceeds normally
        self.abort_builds.store(false, Ordering::Relaxed);
        // 4) reset the in-memory worklist to pristine queued (bar regresses to 0 built / 0 failed)
        {
            let mut sh = self.shared.lock().unwrap();
            let slugs: Vec<String> = sh.results.keys().cloned().collect();
            sh.queue.clear();
            for slug in &slugs {
                if let Some(r) = sh.results.get_mut(slug) {
                    r.status = "queued".into();
                    r.queued_kind = "rebuild".into();
                    r.error.clear();
                }
                sh.queue.push_back(slug.clone());
            }
            sh.packaged.clear();
            sh.cached_cohorts.clear(); // the venvs are about to be deleted; recreated on demand from cohort_reqs
            // the fontspector/ results are about to be wiped — clear the QA de-dup set + queue so rebuilt
            // families are re-QA'd this session (else enqueue_qa skips anything still in qa_done)
            sh.qa_done.clear();
            sh.qa_queue.clear();
            sh.fontspector = None;
        }
        // 5) wipe disk (off-lock — can be many GB). `total` freezes the live progress bar's denominator.
        let total = dir_size(&bd).saturating_add(dir_size(&tools));
        self.shared.lock().unwrap().reset_progress.insert("all".into(), (0, total));
        let mut freed = 0u64;
        let me2 = Arc::clone(self);
        let mut last_pub = std::time::Instant::now();
        let mut publish = move |fr: u64| {
            if last_pub.elapsed() >= Duration::from_millis(200) {
                last_pub = std::time::Instant::now();
                me2.shared.lock().unwrap().reset_progress.insert("all".into(), (fr, total));
            }
        };
        // every entry under the build dir EXCEPT the daemon's own operational files (pid/control), which
        // the still-running daemon needs; logs/ is recreated empty afterwards (every build needs it).
        if let Ok(entries) = std::fs::read_dir(&bd) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name == "daemon.pid" || name == "control.json" {
                    continue;
                }
                let p = e.path();
                if p.is_dir() {
                    remove_tree_progress(&p, &mut freed, &mut publish);
                } else if let Ok(m) = std::fs::metadata(&p) {
                    freed += m.len();
                    let _ = std::fs::remove_file(&p);
                    publish(freed);
                }
            }
        }
        if tools.is_dir() {
            remove_tree_progress(&tools, &mut freed, &mut publish);
        }
        let _ = std::fs::create_dir_all(bd.join("logs"));
        // 6) persist the cleared state, refresh sizes, leave PAUSED (do NOT resume)
        self.save_state();
        let portions = self.compute_reset_portions();
        let now = self.elapsed();
        let mut sh = self.shared.lock().unwrap();
        sh.reset_progress.remove("all");
        sh.reset_portions = portions;
        let msg = format!("✓ deleted everything (freed {}) — PAUSED; resume to rebuild from scratch (archive kept)",
            crate::util::human(freed));
        sh.reset_notes.insert("all".into(), (msg.clone(), now));
        sh.control_log.push(format!("delete everything: {}", msg));
    }

    /// Content signature of the gflib-build override config.yaml (+ this family's build_rules entry) for
    /// `slug`. Empty unless google/fonts has an override carrying OVERRIDE_MARKER — so only families we've
    /// written a fix for are tracked, never ones with a natural upstream config.yaml.
    fn config_sig(&self, slug: &str) -> String {
        let Some(gf) = self.cfg.google_fonts.as_ref() else {
            return String::new();
        };
        let txt = std::fs::read_to_string(gf.join(slug).join("config.yaml")).unwrap_or_default();
        config_signature(&txt, self.build_rules.get(slug))
    }

    /// Auto-rebuild watcher: when a FAILED family's override-config signature changes (we wrote or edited a
    /// fix), re-queue it — the hands-free equivalent of the "retry config-fixed" button. Re-queues only on
    /// an actual signature change, so a still-failing build never loops; a removed override (empty sig) is
    /// left alone. On the first run after a fix is written, the persisted sig is stale ("") so it fires once.
    fn spawn_config_watcher(self: &Arc<Self>) {
        if self.cfg.dry_run
            || self.cfg.google_fonts.is_none()
            || std::env::var_os("GFLIB_NO_AUTO_REBUILD").is_some()
        {
            return;
        }
        let me = Arc::clone(self);
        thread::spawn(move || {
            while !me.stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(4));
                let failed: Vec<(String, String)> = {
                    let sh = me.shared.lock().unwrap();
                    if sh.paused {
                        continue;
                    }
                    sh.results
                        .values()
                        .filter(|r| r.status == "failed")
                        .map(|r| (r.slug.clone(), r.config_sig.clone()))
                        .collect()
                };
                let changed: Vec<String> = failed
                    .into_iter()
                    .filter(|(slug, old)| {
                        let cur = me.config_sig(slug);
                        cur != *old && !cur.is_empty()
                    })
                    .map(|(slug, _)| slug)
                    .collect();
                if changed.is_empty() {
                    continue;
                }
                let mut n = 0;
                {
                    let mut sh = me.shared.lock().unwrap();
                    for slug in &changed {
                        if let Some(r) = sh.results.get_mut(slug) {
                            if r.status != "failed" {
                                continue; // re-check under the lock — may have been re-queued meanwhile
                            }
                            r.status = "queued".into();
                            r.queued_kind = "rebuild".into();
                            r.error.clear();
                            sh.queue.push_front(slug.clone()); // requested rebuild → jump the queue
                            n += 1;
                        }
                    }
                    if n > 0 {
                        sh.control_log.push(format!(
                            "auto-rebuild: {} failed families whose override config changed",
                            n
                        ));
                    }
                }
                if n > 0 {
                    me.cond.notify_all();
                }
            }
        });
    }

    /// Apply an untrusted control message (clamped) to the running build.
    pub fn apply_live(self: &Arc<Self>, set: &ControlSet) {
        let mut log = Vec::new();
        let mut new_jobs = None;
        // Every live-editable setting changed here is persisted to gflib-build.config so the choice
        // survives a daemon reload — user choices are permanent, not silently reverted to launch defaults.
        let mut persist: std::collections::BTreeMap<String, serde_json::Value> = std::collections::BTreeMap::new();
        {
            let mut sh = self.shared.lock().unwrap();
            if let Some(j) = set.jobs {
                let j = j.clamp(0, MAX_JOBS); // 0 = drain: start no new builds, let in-flight finish
                sh.jobs = j;
                self.job_limit.store(j, Ordering::Relaxed); // regulator reads this lock-free
                new_jobs = Some(j);
                persist.insert("jobs".into(), serde_json::json!(j));
                log.push(if j == 0 {
                    "jobs → 0 (drain — no new builds start; in-flight builds finish)".into()
                } else {
                    format!("jobs → {}", j)
                });
            }
            if let Some(p) = set.percent {
                let np = p.clamp(0.0, 100.0);
                let old = sh.percent;
                sh.percent = np;
                // percent is deliberately NOT persisted/forwarded across a reload: it controls worklist
                // MEMBERSHIP (live edits only ADD families, never remove), so re-sampling at a lowered
                // percent on restart would drop — and erase the state.json results of — families the live
                // session already admitted. It stays a per-session setting.
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
                self.frozen.store(pause, Ordering::Relaxed); // regulate() freezes ALL while paused
                log.push(if pause {
                    "paused — froze running builds".into()
                } else {
                    "resumed — thawed builds".into()
                });
            }
            if let Some(b) = &set.backend {
                sh.backend = b.clone();
                persist.insert("backend".into(), serde_json::json!(b));
                log.push(format!("backend → {}", b));
            }
            if let Some(o) = &set.orchestrator {
                sh.orchestrator = o.clone();
                persist.insert("orchestrator".into(), serde_json::json!(o));
                // builder3 = pure fontc, no builder2/fontmake fallback (see attempt_chain)
                log.push(format!("orchestrator → {}", o));
            }
            if let Some(c) = set.compare {
                sh.compare = c;
                persist.insert("compare".into(), serde_json::json!(c));
                log.push(format!("compare → {}", if c { "on" } else { "off" }));
            }
            if let Some(b) = set.build_debs {
                sh.build_debs = b;
                persist.insert("build_debs".into(), serde_json::json!(b));
                log.push(format!("build .deb packages → {}", if b { "on" } else { "off" }));
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
                        sh.queue.push_front(slug); // requested retry → ahead of not-yet-built families
                    }
                }
                log.push("retry ALL failed".into());
            }
            if set.retry_overrides == Some(true) {
                // re-queue every FAILED family we've written a gflib-build override config.yaml for
                // (the OVERRIDE_MARKER) — e.g. the whole instantiateUfo-bypass set — so a config fix
                // is picked up without retrying genuinely-unfixed failures. (A handful of small reads
                // under the lock, only for a user-initiated button press.)
                let failed: Vec<String> = sh.results.values()
                    .filter(|r| r.status == "failed").map(|r| r.slug.clone()).collect();
                let mut n = 0;
                if let Some(gf) = self.cfg.google_fonts.clone() {
                    for slug in failed {
                        let marked = std::fs::read_to_string(gf.join(&slug).join("config.yaml"))
                            .map(|t| t.contains(OVERRIDE_MARKER)).unwrap_or(false);
                        if marked {
                            if let Some(r) = sh.results.get_mut(&slug) {
                                r.status = "queued".into();
                                r.queued_kind = "rebuild".into();
                                r.error.clear();
                                sh.queue.push_front(slug); // requested rebuild → jump the queue
                                n += 1;
                            }
                        }
                    }
                }
                log.push(format!("retry {} override-fixed families", n));
            }
            if set.repackage_all == Some(true) {
                // Clear the de-dup set so the package worker rebuilds every .deb from the existing fonts
                // (no font rebuild) — applying packaging fixes (copyright/changelog) and re-linting each.
                // Ensure build_debs is on, else the worker would idle instead of packaging.
                let n = sh.packaged.len();
                sh.packaged.clear();
                if !sh.build_debs {
                    sh.build_debs = true;
                    persist.insert("build_debs".into(), serde_json::json!(true));
                }
                log.push(format!("repackage all — cleared {} packaged markers; the worker will rebuild every .deb", n));
            }
            for l in &log {
                sh.control_log.push(l.clone());
            }
            let n = sh.control_log.len();
            if n > 200 {
                sh.control_log.drain(0..n - 200);
            }
        }
        // Reconcile freeze/thaw AFTER releasing the shared lock: the regulator enforces the live
        // (paused, jobs) target — a global pause freezes everything; otherwise it keeps exactly `jobs`
        // builds unfrozen, freezing the newest excess when jobs was lowered and thawing the oldest when
        // it was raised. (regulate() takes only the `running` lock, never `sh`, so this is deadlock-free.)
        self.regulate();
        // persist every live setting the user just changed (backend/jobs/compare/build_debs) so the next
        // run — or a daemon reload — agrees on disk instead of reverting to launch defaults. NEVER in
        // dry-run: the mockup promises "nothing written to disk", and self.cfg.data_dir is the REAL dir.
        if !persist.is_empty() && !self.cfg.dry_run {
            let path = self.cfg.data_dir.join("gflib-build.config");
            let _ = crate::config::save_config_map(&path, &persist);
        }
        if let Some(j) = new_jobs {
            self.ensure_workers(j);
        }
        self.cond.notify_all();
        if set.restart == Some(true) {
            self.restart_self(); // exec's the daemon — does not return on success
        }
    }

    fn spawn_status_writer(self: &Arc<Self>) {
        let me = Arc::clone(self);
        thread::spawn(move || {
            let mut tick: u64 = 0;
            while !me.stop.load(Ordering::Relaxed) {
                let mut snap = me.snapshot();
                snap.fontspector = None; // kept out of status.json (large); monitors overlay _summary.json
                persist::write_status(&me.cfg.build_dir, &snap);
                if tick % 10 == 0 {
                    // derived reports consumed by the dashboard (refreshed every ~10 s, not every tick)
                    persist::write_json_file(&me.cfg.build_dir, "migration.json", &me.migration_json());
                    persist::write_json_file(&me.cfg.build_dir, "timings.json", &me.timings_json());
                }
                tick += 1;
                thread::sleep(Duration::from_millis(1000));
            }
            // one last write so a monitor sees the final state
            let mut snap = me.snapshot();
            snap.fontspector = None;
            persist::write_status(&me.cfg.build_dir, &snap);
            persist::write_json_file(&me.cfg.build_dir, "migration.json", &me.migration_json());
            persist::write_json_file(&me.cfg.build_dir, "timings.json", &me.timings_json());
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
                // reset-tab portion sizes: a second (bucketed) walk, every ~30 s
                let portions = if tick % 3 == 0 { Some(me.compute_reset_portions()) } else { None };
                {
                    let mut sh = me.shared.lock().unwrap();
                    sh.disk_build_total = build_total;
                    sh.disk_free = free;
                    sh.cached_cohorts = cached;
                    if let Some(p) = portions {
                        sh.reset_portions = p;
                    }
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

    fn spawn_package_worker(self: &Arc<Self>) {
        let me = Arc::clone(self);
        thread::spawn(move || me.package_worker_loop());
    }

    /// Live incremental packaging (the auto path that retires manual `--export-deb`): while build_debs
    /// is on, draft + build + validate the .deb for each built family not yet packaged, writing
    /// packaging/index.json + build-results.json as it goes. Parks while paused (frozen) so a
    /// resource-freeing pause also pauses packaging.
    fn package_worker_loop(self: Arc<Self>) {
        if self.cfg.dry_run {
            return; // the mockup writes nothing persistent
        }
        let pkg_root = self.cfg.build_dir.join("packaging");
        // Append to any prior index/results across restarts; seed `packaged` so we don't redo them.
        let mut index = read_pkg_index(&pkg_root);
        let mut results = read_deb_results(&pkg_root);
        {
            // Seed `packaged` from prior TERMINAL results only — a real .deb (built==true) or a recorded
            // no-fonts skip. NOT from index.json: a draft-only `--export-deb` (without --build-debs) fills
            // index.json with no .debs built, and seeding those would wrongly suppress building them.
            let mut sh = self.shared.lock().unwrap();
            for (k, v) in results.iter() {
                let built = v.get("built").and_then(|b| b.as_bool()).unwrap_or(false);
                let no_fonts = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.contains("no fonts"))
                    .unwrap_or(false);
                if built || no_fonts {
                    sh.packaged.insert(k.clone());
                }
            }
        }
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            if self.frozen.load(Ordering::Relaxed) {
                self.shared.lock().unwrap().pkg_now.clear(); // parked: not packaging — keep pkg_now honest
                thread::sleep(Duration::from_millis(300)); // paused: also pause packaging
                continue;
            }
            // pick the next built-but-unpackaged family (only while build_debs is on)
            let pick = {
                let sh = self.shared.lock().unwrap();
                if !sh.build_debs {
                    None
                } else {
                    sh.results
                        .values()
                        .find(|r| r.status == "built" && !sh.packaged.contains(&r.slug))
                        .map(|r| r.slug.clone())
                }
            };
            let slug = match pick {
                Some(s) => s,
                None => {
                    // No fresh family to package. Retroactively lint one already-built .deb whose lintian
                    // step never ran — so installing lintian after the fact still covers the whole backlog
                    // of already-"validated" packages. Sleep only when there's nothing left to lint.
                    if !self.relint_one(&pkg_root, &mut results) {
                        self.shared.lock().unwrap().pkg_now.clear(); // truly idle: nothing to package or lint
                        thread::sleep(Duration::from_millis(500));
                    }
                    continue;
                }
            };
            self.shared.lock().unwrap().pkg_now = format!("packaging {}", slug); // publish activity for the queue view
            // gather inputs under the lock, then package OUTSIDE it (dpkg-deb is slow)
            let (res, fam, cohort_req) = {
                let sh = self.shared.lock().unwrap();
                let res = sh.results.get(&slug).cloned();
                let fam = sh.families.get(&slug).cloned();
                let creq = res.as_ref().and_then(|r| sh.cohort_reqs.get(&r.cohort).cloned());
                (res, fam, creq)
            };
            let (res, fam) = match (res, fam) {
                (Some(r), Some(f)) => (r, f),
                _ => {
                    self.shared.lock().unwrap().packaged.insert(slug.clone());
                    continue;
                }
            };
            let gen = res.ended; // build generation: detect a rebuild that lands while we package
            let lint = crate::deb::on_path("lintian");
            let o = crate::deb::package_one_family(
                &self.cfg.build_dir,
                &slug,
                &res,
                &fam,
                self.cfg.google_fonts.as_deref(),
                cohort_req.as_ref(),
                true,
                lint,
            );
            // built but its out/ fonts were pruned (--discard-fonts before build_debs was on): record a
            // terminal failure so it's visible in deb_status and not re-attempted every restart.
            if o.skipped == Some("no_fonts") {
                results.insert(slug.clone(), serde_json::json!({
                    "built": false,
                    "no_fonts": true, // not a dpkg-deb failure — packaging skipped because the fonts are gone
                    "error": "no fonts on disk (rebuild with .deb packaging enabled to keep them)",
                }));
                self.shared.lock().unwrap().packaged.insert(slug.clone());
                write_deb_results(&pkg_root, &results);
                continue;
            }
            if o.skipped.is_some() {
                self.shared.lock().unwrap().packaged.insert(slug.clone()); // mkdir/write fail: don't spin
                continue;
            }
            // Commit ONLY if the family wasn't rebuilt while we packaged it — else the .deb we just built
            // is stale; leave it unpackaged so the next iteration repackages the fresh build.
            let still_current = {
                let mut sh = self.shared.lock().unwrap();
                let ok = sh
                    .results
                    .get(&slug)
                    .map(|r| r.status == "built" && r.ended == gen)
                    .unwrap_or(false);
                if ok {
                    sh.packaged.insert(slug.clone());
                }
                ok
            };
            if still_current {
                if let Some(e) = o.index_entry {
                    index.insert(slug.clone(), e);
                }
                if let Some(d) = o.deb_result {
                    results.insert(slug.clone(), d);
                }
                write_pkg_index(&pkg_root, &index);
                write_deb_results(&pkg_root, &results);
            }
        }
    }

    /// Retroactively lint ONE already-built .deb whose lintian step never ran (lint missing or
    /// "not run (lintian absent)"). No rebuild — runs lintian on the existing .deb and records the
    /// result, so a backlog of "validated" packages becomes "lint-clean"/warnings once lintian is
    /// installed. Returns true if it processed one (so the worker keeps going without sleeping).
    fn relint_one(&self, pkg_root: &Path, results: &mut serde_json::Map<String, serde_json::Value>) -> bool {
        if !crate::deb::on_path("lintian") {
            return false; // lintian not installed yet — nothing to do (the toolchain panel flags it)
        }
        // a result needs (re-)linting if it built a .deb but has no recorded lint_tags breakdown yet —
        // this covers BOTH packages linted when lintian was absent AND ones linted by an older binary
        // that recorded a verdict but no tags. A real lint (or a terminal failure) always writes
        // lint_tags, so processed packages drop out and the sweep converges.
        let pick = results.iter().find_map(|(slug, r)| {
            if !r.get("built").and_then(|b| b.as_bool()).unwrap_or(false) {
                return None;
            }
            if r.get("lint_tags").is_some() {
                return None; // already has a tag breakdown (linted, or marked terminal)
            }
            let pkg = r.get("package").and_then(|v| v.as_str())?;
            let ver = r.get("version").and_then(|v| v.as_str())?;
            Some((slug.clone(), pkg.to_string(), ver.to_string()))
        });
        let (slug, pkg, ver) = match pick {
            Some(x) => x,
            None => return false,
        };
        self.shared.lock().unwrap().pkg_now = format!("linting {}", slug); // publish activity for the queue view
        let pool = pkg_root.join("pool");
        // None => the .deb is gone (or lintian wouldn't spawn); record an empty tag set so the package is
        // marked terminal (lint_tags present) and we don't spin on it forever.
        let (verdict, tags) = crate::deb::relint_deb(&pool, &pkg, &ver)
            .unwrap_or_else(|| ("no .deb on disk".into(), Vec::new()));
        if let Some(obj) = results.get_mut(&slug).and_then(|v| v.as_object_mut()) {
            obj.insert("lint".into(), serde_json::json!(verdict));
            obj.insert("lint_tags".into(), serde_json::json!(tags));
        }
        write_deb_results(pkg_root, results);
        true
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // thaw then kill any in-flight builder groups so frozen/running children don't orphan when the
        // daemon exits (the children are process-group leaders, so they survive the daemon otherwise).
        self.signal_running(SIGCONT);
        self.signal_running(SIGKILL);
        self.cond.notify_all();
    }

    pub fn restart_requested(&self) -> bool {
        self.restart_requested.load(Ordering::SeqCst)
    }

    /// The UI "Restart" button. Routes through the SAME graceful shutdown SIGTERM/`--stop` uses (set the
    /// flag the daemon loop polls) plus a restart marker; run_daemon then finalizes, clears the pidfile,
    /// and RE-SPAWNS a fresh daemon. We do NOT re-exec in place — in the monitor/daemon split that fought
    /// the monitor's web port and could leave the daemon frozen. If the re-spawn fails the daemon simply
    /// stays stopped (re-run manually) — never bricked. (Foreground non-detached daemons just stop.)
    fn restart_self(self: &Arc<Self>) {
        if self.cfg.dry_run {
            return; // the mockup never touches the real session
        }
        self.shared.lock().unwrap().control_log.push(
            "restart requested via the UI — finishing the current snapshot, then re-launching the daemon".into());
        self.save_state();
        self.restart_requested.store(true, Ordering::SeqCst);
        crate::daemon::request_sigterm(); // run_daemon's loop polls this → clean finalize + re-spawn
        self.cond.notify_all();
    }

    /// The live-editable settings as CLI flags, to APPEND on a UI "Restart" re-exec. The restart re-passes
    /// the original argv verbatim, so a launch-time `--backend auto` / `--jobs 10` would otherwise override
    /// the user's later UI choices (and the persisted config). The parser is last-wins, so appending the
    /// CURRENT values makes the fresh daemon come up exactly as the UI shows it — user choices are permanent.
    pub fn live_overrides_argv(&self) -> Vec<std::ffi::OsString> {
        use std::ffi::OsString;
        let sh = self.shared.lock().unwrap();
        let mut v: Vec<OsString> = Vec::new();
        if !sh.backend.is_empty() {
            v.push("--backend".into());
            v.push(sh.backend.clone().into());
        }
        if !sh.orchestrator.is_empty() {
            v.push("--orchestrator".into());
            v.push(sh.orchestrator.clone().into());
        }
        // jobs as-is (0 = inspect-only; preserve it rather than forcing a build)
        v.push("--jobs".into());
        v.push(sh.jobs.to_string().into());
        // NOTE: --percent is intentionally omitted — re-narrowing the sample on restart would drop families
        // (and their state.json results) the live session already admitted. percent stays per-session.
        v.push(if sh.compare { "--compare" } else { "--no-compare" }.into());
        v.push(if sh.build_debs { "--build-debs" } else { "--no-build-debs" }.into());
        v
    }

    /// Send `sig` to every in-flight builder process group regardless of freeze state — used to reap
    /// children (SIGCONT then SIGKILL) on stop, so nothing orphans.
    fn signal_running(&self, sig: i32) {
        let reg = self.running.lock().unwrap();
        for pgid in reg.pgids() {
            signal_group(pgid, sig);
        }
    }

    /// Job regulator: bring the set of ACTIVELY-running (unfrozen) builder children in line with the
    /// live target — exactly `job_limit` unfrozen when running, or all frozen when globally paused.
    /// Lowering jobs below the number of running builds freezes (SIGSTOP) the newest excess; as builds
    /// finish, the freed slots thaw (SIGCONT) the oldest frozen build first, so in-progress work drains
    /// before any new family starts. Signals under the `running` lock (matching the register path) so a
    /// concurrent finish/register can't interleave a freeze against a just-reused pgid.
    fn regulate(&self) {
        let paused = self.frozen.load(Ordering::Relaxed);
        let jobs = self.job_limit.load(Ordering::Relaxed);
        let mut reg = self.running.lock().unwrap();
        let (freeze, thaw) = reg.plan(paused, jobs);
        for pgid in freeze {
            signal_group(pgid, SIGSTOP);
        }
        for pgid in thaw {
            signal_group(pgid, SIGCONT);
        }
    }

    /// Flush the final status + derived reports SYNCHRONOUSLY (so a short foreground run doesn't exit
    /// before the background status-writer thread gets to write them).
    pub fn finalize(&self) {
        if self.cfg.dry_run {
            return; // the mockup writes nothing persistent
        }
        // a final QA aggregate so the summary reflects every per-family result, even if the periodic
        // aggregator's last tick was before the last family finished (or the process exits promptly)
        if self.cfg.fontspector_qa {
            if let Some((_, version)) = self.qa_bin.lock().unwrap().clone() {
                let fsdir = crate::persist::fontspector_dir(&self.cfg.build_dir);
                let view = crate::fontspector::aggregate(&fsdir, &self.cfg.fontspector_profile, &version);
                let _ = std::fs::write(fsdir.join("_summary.json"), serde_json::to_string(&view).unwrap_or_default());
                self.shared.lock().unwrap().fontspector = Some(view);
            }
        }
        let mut snap = self.snapshot();
        snap.fontspector = None;
        persist::write_status(&self.cfg.build_dir, &snap);
        persist::write_json_file(&self.cfg.build_dir, "migration.json", &self.migration_json());
        persist::write_json_file(&self.cfg.build_dir, "timings.json", &self.timings_json());
    }

    /// Build the live snapshot rendered by every frontend and written to status.json.
    pub fn snapshot(&self) -> Snapshot {
        // in-flight builder children + how many are currently frozen (by a pause or the job limit) —
        // read before the central lock
        let (running_builds, frozen_builds) = {
            let reg = self.running.lock().unwrap();
            (reg.len(), reg.frozen_count())
        };
        // packaging status: read packaging/ ONCE, BEFORE taking the central lock, so the readdir
        // never stalls other threads on the mutex. "drafted" = a packaging/<slug__>/ directory
        // exists (keep only real subdirectories, skipping index.json).
        let drafted: std::collections::HashSet<String> =
            std::fs::read_dir(self.cfg.build_dir.join("packaging"))
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
        // deb-build external toolchain (cached 5s; recovers as tools are installed). Computed before
        // the lock — it has its own cache mutex, no central-lock dependency.
        let deb_tools = crate::deb::deb_tools_cached();
        // per-package deb-build status from packaging/build-results.json (read once, before the lock)
        let deb_obj: Option<serde_json::Map<String, serde_json::Value>> = std::fs::read_to_string(
            self.cfg.build_dir.join("packaging").join("build-results.json"),
        )
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("results").and_then(|r| r.as_object()).cloned());
        let deb_results: std::collections::HashMap<String, (String, String)> = deb_obj
            .as_ref()
            .map(|obj| {
                obj.iter()
                    .map(|(slug, res)| {
                        let built = res.get("built").and_then(|b| b.as_bool()).unwrap_or(false);
                        let validated = res.get("validated").and_then(|b| b.as_bool()).unwrap_or(false);
                        let lint = res.get("lint").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        // "no fonts on disk" is NOT a dpkg-deb failure — the family's binaries were
                        // discarded so packaging was never attempted. Detect it (explicit flag, with an
                        // error-string fallback for entries written before the flag existed) and report it
                        // as its own non-failure state so the bar shows "not packaged", not "deb-failed".
                        // match the EXACT legacy phrase (entries written before the no_fonts flag), not a
                        // broad "no fonts" substring — so a genuine failure like "no fonts to package" is
                        // never misread as the discarded-fonts skip.
                        let no_fonts = res.get("no_fonts").and_then(|b| b.as_bool()).unwrap_or(false)
                            || res.get("error").and_then(|e| e.as_str())
                                .map(|e| e.starts_with("no fonts on disk")).unwrap_or(false);
                        // once lintian has run, the lintian verdict supersedes "validated":
                        //   errors   -> lintian-fail    (a regression — dpkg-deb ok, but lintian found errors)
                        //   warnings -> lint-warn        (passed lintian, no errors, but with warnings)
                        //   clean    -> lint-clean       (passed lintian with nothing at all)
                        //   not run  -> validated        (dpkg-deb ok; lintian hasn't run yet)
                        let st = if validated {
                            if lint.contains("error") { "lintian-fail" }
                            else if lint == "clean" { "lint-clean" }
                            else if lint.contains("warning") { "lint-warn" }
                            else { "validated" }
                        } else if built { "built" }
                          else if no_fonts { "no-fonts" } // fonts discarded → not packaged, not failed
                          else { "failed" };
                        (slug.clone(), (st.to_string(), lint))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // group lintian findings by (severity, tag) across all packages — the packaging analogue of
        // fail_categories. Each package contributes once per distinct tag it carries.
        let lint_categories: Vec<crate::model::LintCategory> = {
            let mut cat: BTreeMap<(String, String), (usize, Vec<String>)> = BTreeMap::new();
            if let Some(obj) = deb_obj.as_ref() {
                for (slug, res) in obj.iter() {
                    let tags = match res.get("lint_tags").and_then(|v| v.as_array()) {
                        Some(t) => t,
                        None => continue,
                    };
                    for t in tags {
                        let arr = match t.as_array() {
                            Some(a) => a,
                            None => continue,
                        };
                        let sev = arr.first().and_then(|x| x.as_str()).unwrap_or("");
                        let tag = arr.get(1).and_then(|x| x.as_str()).unwrap_or("");
                        if tag.is_empty() {
                            continue;
                        }
                        let e = cat.entry((sev.to_string(), tag.to_string())).or_insert((0, Vec::new()));
                        e.0 += 1;
                        e.1.push(slug.clone());
                    }
                }
            }
            let mut v: Vec<crate::model::LintCategory> = cat
                .into_iter()
                .map(|((severity, tag), (count, families))| crate::model::LintCategory { tag, severity, count, families })
                .collect();
            // errors before warnings, then most-affected first
            v.sort_by(|a, b| (b.severity == "E").cmp(&(a.severity == "E")).then(b.count.cmp(&a.count)));
            v
        };
        // lint queue progress: lintable = packages with a .deb; done = those lintian has run on (lint_tags present)
        let (lint_total, lint_done) = deb_obj
            .as_ref()
            .map(|obj| {
                let mut total = 0usize;
                let mut done = 0usize;
                for (_s, r) in obj.iter() {
                    if r.get("built").and_then(|b| b.as_bool()).unwrap_or(false) {
                        total += 1;
                        if r.get("lint_tags").is_some() {
                            done += 1;
                        }
                    }
                }
                (total, done)
            })
            .unwrap_or((0, 0));
        let lint_pending = lint_total.saturating_sub(lint_done);
        let sh = self.shared.lock().unwrap();
        let mut counts = Counts::default();
        let mut backends = Backends::default();
        let mut migration: BTreeMap<String, usize> = BTreeMap::new();
        let mut python_versions: BTreeMap<String, usize> = BTreeMap::new(); // built families by interpreter
        let frozen_slugs = self.running.lock().unwrap().frozen_slugs(); // which in-flight builds are SIGSTOP-frozen
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
                if !r.python_version.is_empty() {
                    *python_versions.entry(r.python_version.clone()).or_insert(0) += 1;
                }
                match r.backend.as_str() {
                    "fontc" => {
                        backends.fontc += 1;
                        *migration.entry("fontc".into()).or_default() += 1;
                    }
                    "fontmake" => {
                        backends.fontmake += 1;
                        *migration.entry("fontmake_fallback".into()).or_default() += 1;
                    }
                    "both" => {
                        backends.both += 1;
                        *migration.entry("both".into()).or_default() += 1;
                    }
                    _ => {}
                }
                // the M5 (Python-free) count: families whose ORCHESTRATOR was builder3
                if r.builder == "builder3" {
                    *migration.entry("builder3".into()).or_default() += 1;
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
                    python_version: r.python_version.clone(),
                    packaged: drafted.contains(&r.slug.replace('/', "__")),
                    deb_status: deb_results.get(&r.slug).map(|p| p.0.clone()).unwrap_or_default(),
                    deb_lint: deb_results.get(&r.slug).map(|p| p.1.clone()).unwrap_or_default(),
                    crater: self.crater_by_slug.get(&r.slug).map(|s| s.token()).unwrap_or_default(),
                });
            }
            if r.status == "building" {
                building.push(BuildingItem {
                    slug: r.slug.clone(),
                    worker: r.worker,
                    dur: now() - r.started,
                    backend: r.backend.clone(),
                    note: r.note.clone(),
                    frozen: frozen_slugs.contains(&r.slug),
                });
            }
            if r.status == "queued" {
                queued_list.push(QueuedItem {
                    slug: r.slug.clone(),
                    kind: if r.queued_kind.is_empty() { "new".into() } else { r.queued_kind.clone() },
                    crater: self.crater_by_slug.get(&r.slug).map(|s| s.token()).unwrap_or_default(),
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
                    crater: self.crater_by_slug.get(&r.slug).map(|s| s.token()).unwrap_or_default(),
                    rebuild_note: crate::classify::rebuild_pending_note("failed", "", &r.error)
                        .unwrap_or_default(),
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
        let packages = built.clone(); // full, uncapped — the packaging tab needs every built family
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
                // each member as {display NAME (from METADATA.pb, falling back to slug), build STATUS}
                // — the name matches Python's _rebuild_cohorts; the status lets both UIs colour it
                families: {
                    let mut fl: Vec<crate::model::CohortFam> = fams
                        .iter()
                        .map(|s| {
                            let name = sh.families.get(s).map(|f| f.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| s.clone());
                            let status = sh.results.get(s).map(|r| r.status.clone()).filter(|st| !st.is_empty()).unwrap_or_else(|| "pending".into());
                            crate::model::CohortFam { name, status }
                        })
                        .collect();
                    fl.sort_by(|a, b| a.name.cmp(&b.name));
                    fl
                },
                cached: sh.cached_cohorts.contains(key),
            })
            .collect();
        cohorts_out.sort_by(|a, b| b.count.cmp(&a.count));

        let mut fail_hist: Vec<FailHist> = sh.failure_history.iter().rev().take(400).cloned().collect();
        fail_hist.reverse();

        // done = nothing queued, nothing building, no worker in flight (correct with 0 families so a
        // daemon idle-exits; the active counter guards the build→built window). Computed before the
        // struct literal moves `counts`. When QA is on, also wait for the QA queue to drain so the
        // daemon doesn't exit mid-QA.
        let qa_pending = self.cfg.fontspector_qa
            && (!sh.qa_init || !sh.qa_queue.is_empty() || !sh.qa_active.is_empty());
        // when auto-packaging is on, also wait for the package backlog so a headless/foreground run
        // doesn't request_stop (killing the package worker) with built-but-unpackaged families left.
        let pkg_pending = sh.build_debs
            && sh.results.values().any(|r| r.status == "built" && !sh.packaged.contains(&r.slug));
        let done = counts.queued == 0 && counts.building == 0
            && self.active.load(Ordering::Relaxed) == 0 && !qa_pending && !pkg_pending;

        let op_stats: BTreeMap<String, OpStat> = sh
            .op_stats
            .iter()
            .map(|(op, (total, count, max))| {
                let r2 = |x: f64| (x * 100.0).round() / 100.0;
                (op.clone(), OpStat {
                    total: r2(*total),
                    count: *count,
                    mean: if *count > 0 { (total / *count as f64 * 1000.0).round() / 1000.0 } else { 0.0 },
                    max: r2(*max),
                })
            })
            .collect();

        let archive = ArchiveView {
            total: sh.archive_total,
            active: sh.archive_active.iter().map(|u| repo_slug(u)).collect(),
            pending: sh.archive_pending.iter().take(60).map(|u| repo_slug(u)).collect(),
            pending_total: sh.archive_pending.len(),
            recent: sh.archive_recent.iter().rev().take(40).cloned().collect(),
        };

        // build-tool packages (the Python->Rust / M5 burn-down): aggregate each family's dependency
        // requirements + its compiler/orchestrator into a tool -> dependent-families map. CPU-only
        // (no I/O) so it does not extend the lock with a stall.
        let tool_packages = {
            let cohort_pkgs: BTreeMap<&String, Vec<String>> = sh
                .cohort_reqs
                .iter()
                .map(|(k, v)| (k, v.lines().map(crate::venv::req_pkg_name).filter(|p| !p.is_empty()).collect()))
                .collect();
            let mut tf: BTreeMap<String, (std::collections::BTreeSet<String>, &'static str)> = BTreeMap::new();
            for r in sh.results.values() {
                if let Some(pkgs) = cohort_pkgs.get(&r.cohort) {
                    for pkg in pkgs {
                        tf.entry(pkg.clone())
                            .or_insert_with(|| (std::collections::BTreeSet::new(), "requirement"))
                            .0
                            .insert(r.slug.clone());
                    }
                }
                let comps: Vec<&str> = match r.backend.as_str() {
                    "both" => vec!["fontc", "fontmake"],
                    "" => vec![],
                    b => vec![b],
                };
                for c in comps {
                    tf.entry(c.to_string())
                        .or_insert_with(|| (std::collections::BTreeSet::new(), "compiler"))
                        .0
                        .insert(r.slug.clone());
                }
                if !r.builder.is_empty() {
                    tf.entry(r.builder.clone())
                        .or_insert_with(|| (std::collections::BTreeSet::new(), "orchestrator"))
                        .0
                        .insert(r.slug.clone());
                }
            }
            let rust_tools: std::collections::HashSet<&str> =
                ["fontc", "builder3", "gftools-builder3"].into_iter().collect();
            let mut v: Vec<crate::model::ToolPkg> = tf
                .into_iter()
                .map(|(name, (fams, kind))| {
                    let lang = if name == "builder2" || name == "fontmake" {
                        "python"
                    } else if rust_tools.contains(name.as_str()) {
                        "rust"
                    } else {
                        "python"
                    };
                    let family_list: Vec<String> = fams.iter().take(300).cloned().collect();
                    crate::model::ToolPkg {
                        name,
                        lang: lang.to_string(),
                        kind: kind.to_string(),
                        families: fams.len(),
                        family_list,
                        packaged: false,
                    }
                })
                .collect();
            v.sort_by(|a, b| b.families.cmp(&a.families).then(a.name.cmp(&b.name)));
            v
        };

        // fontc_crater comparison summary: how our build outcomes line up with crater's latest
        // verdict, family by family. The actionable buckets are the gold (we build / fontc can't)
        // and the regressions (we fail / fontc built it).
        let crater_view = self.crater.as_ref().map(|c| {
            use crate::crater::CraterStatus as CS;
            let mut v = crate::model::CraterView {
                run: c.meta.latest_run.clone(),
                fontc_rev: c.meta.fontc_rev.clone(),
                fonts_repo_sha: c.meta.fonts_repo_sha.clone(),
                complete: c.meta.complete,
                ..Default::default()
            };
            for r in sh.results.values() {
                let st = match self.crater_by_slug.get(&r.slug) {
                    Some(s) => s,
                    None => continue,
                };
                v.matched += 1;
                match st {
                    CS::Identical => v.c_identical += 1,
                    CS::Diff(_) => v.c_diff += 1,
                    CS::FontcFailed => v.c_fontc_failed += 1,
                    CS::FontmakeFailed => v.c_fontmake_failed += 1,
                    CS::BothFailed => v.c_both_failed += 1,
                    CS::RepoFailed => v.c_repo_failed += 1,
                }
                let built = r.status == "built";
                if built && st.fontc_failed() {
                    v.we_build_fontc_cant += 1;
                    if v.gold_families.len() < 60 {
                        v.gold_families.push(r.slug.clone());
                    }
                }
                if r.status == "failed" && st.fontc_built() {
                    v.we_fail_fontc_ok += 1;
                    if v.regression_families.len() < 60 {
                        v.regression_families.push(r.slug.clone());
                    }
                }
                if built {
                    match st {
                        CS::Identical => v.both_ok_identical += 1,
                        CS::Diff(_) => v.both_ok_diff += 1,
                        _ => {}
                    }
                }
            }
            v.gold_families.sort();
            v.regression_families.sort();
            v
        });

        // built families still awaiting (re)packaging = not in the live `packaged` de-dup set. (NOT
        // counts.built - lint_total: the build-results entries persist, so that stays 0 after a repackage
        // is requested even while the worker is rebuilding every .deb.)
        let pkg_pending = sh.results.values().filter(|r| r.status == "built" && !sh.packaged.contains(&r.slug)).count();
        Snapshot {
            elapsed: self.elapsed(),
            disk_used_delta: 0,
            disk_free: sh.disk_free,
            disk_build_total: sh.disk_build_total,
            disk_archive_total: sh.disk_archive_total,
            disk_archive_nested: sh.disk_archive_nested,
            jobs: sh.jobs,
            paused: sh.paused,
            running_builds,
            frozen_builds,
            total: sh.results.len(),
            counts,
            backends,
            building,
            failures_recent: fails,
            built_recent: built,
            packages,
            queued_list,
            fail_categories,
            lint_categories,
            build_debs: sh.build_debs,
            pkg_now: sh.pkg_now.clone(),
            pkg_pending,
            lint_total,
            lint_done,
            lint_pending,
            cohorts_ready: self
                .venvs
                .as_ref()
                .map(|v| v.ready_count())
                .unwrap_or_else(|| cohorts_out.iter().filter(|c| c.cached).count()),
            cohorts: cohorts_out,
            tool_packages,
            deb_tools,
            phase: sh.phase.clone(),
            phase_total: sh.library_total,
            phase_done: 0,
            phase_label: sh.phase_label.clone(),
            phase_error: String::new(),
            failure_history: fail_hist,
            tooling,
            builders,
            migration,
            python_versions,
            op_stats,
            phase_durations: [("build".to_string(), (self.elapsed() * 10.0).round() / 10.0)]
                .into_iter()
                .collect(),
            tasks: self.toolchain_tasks(),
            reset_portions: {
                let mut ps = sh.reset_portions.clone();
                let now = self.elapsed();
                for p in ps.iter_mut() {
                    if let Some((freed, total)) = sh.reset_progress.get(&p.key) {
                        p.deleting = true;
                        p.freed = *freed;
                        p.bytes = *total; // freeze the bar's denominator for the whole deletion
                    }
                    if let Some((msg, at)) = sh.reset_notes.get(&p.key) {
                        if now - at < 6.0 { p.note = msg.clone(); }
                    }
                }
                ps
            },
            archive_recent: Vec::new(),
            archive,
            config: {
                // reflect the LIVE (config-tab-editable) values so the form shows current state
                let mut c = config_map(&self.cfg);
                c.insert("jobs".into(), serde_json::json!(sh.jobs));
                c.insert("percent".into(), serde_json::json!(sh.percent));
                c.insert("backend".into(), serde_json::json!(sh.backend));
                c.insert("orchestrator".into(), serde_json::json!(sh.orchestrator));
                c.insert("compare".into(), serde_json::json!(sh.compare));
                c
            },
            control_log: sh.control_log.clone(),
            dep_relaxations: self.venvs.as_ref().map(|v| v.relaxations()).unwrap_or_default(),
            config_path: self.cfg.data_dir.join("gflib-build.config").to_string_lossy().to_string(),
            pre_build: false, // a live build is never the setup wizard
            fontspector: sh.fontspector.clone(), // live QA aggregate (async --fontspector orchestration)
            crater: crater_view,
            done,
            daemon_alive: true,
        }
    }
}

// ----------------------------------------------------------------- build subroutines

/// Map a repo URL to its bare-mirror path under the archive (ported from Python `mirror_path`).
/// A short "owner/repo" display slug for a git URL (for the archive view).
pub fn repo_slug(repo_url: &str) -> String {
    let mut u = repo_url.trim().trim_end_matches('/').to_string();
    for p in ["https://", "http://"] {
        if let Some(r) = u.strip_prefix(p) {
            u = r.to_string();
        }
    }
    if let Some(idx) = u.find("git@") {
        u = u[idx + 4..].replacen(':', "/", 1);
    }
    if u.ends_with(".git") {
        u.truncate(u.len() - 4);
    }
    let parts: Vec<&str> = u.split('/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        u
    }
}

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

/// The year a commit was authored (from the bare mirror), for the Python-ladder era heuristic.
fn commit_year(mirror: &Path, commit: &str) -> Option<u32> {
    let out = std::process::Command::new("git")
        .args(["-C", &mirror.to_string_lossy(), "show", "-s", "--format=%cd", "--date=format:%Y", commit])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
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

// ---- incremental packaging index/results I/O (the live package worker appends across restarts) ----

fn read_pkg_index(pkg_root: &Path) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut m = std::collections::BTreeMap::new();
    if let Ok(txt) = std::fs::read_to_string(pkg_root.join("index.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(arr) = v.get("packages").and_then(|p| p.as_array()) {
                for e in arr {
                    if let Some(slug) = e.get("slug").and_then(|s| s.as_str()) {
                        m.insert(slug.to_string(), e.clone());
                    }
                }
            }
        }
    }
    m
}

fn write_pkg_index(pkg_root: &Path, index: &std::collections::BTreeMap<String, serde_json::Value>) {
    let _ = std::fs::create_dir_all(pkg_root);
    let packages: Vec<&serde_json::Value> = index.values().collect();
    let doc = serde_json::json!({ "schema_version": 1, "count": packages.len(), "packages": packages });
    if let Ok(txt) = serde_json::to_string_pretty(&doc) {
        atomic_write(&pkg_root.join("index.json"), &txt);
    }
}

fn read_deb_results(pkg_root: &Path) -> serde_json::Map<String, serde_json::Value> {
    if let Ok(txt) = std::fs::read_to_string(pkg_root.join("build-results.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(r) = v.get("results").and_then(|r| r.as_object()) {
                return r.clone();
            }
        }
    }
    serde_json::Map::new()
}

fn write_deb_results(pkg_root: &Path, results: &serde_json::Map<String, serde_json::Value>) {
    let _ = std::fs::create_dir_all(pkg_root);
    let built = results
        .values()
        .filter(|d| d.get("built").and_then(|b| b.as_bool()).unwrap_or(false))
        .count();
    let failed = results.len().saturating_sub(built);
    let doc = serde_json::json!({ "schema_version": 1, "built": built, "failed": failed, "results": results });
    if let Ok(txt) = serde_json::to_string_pretty(&doc) {
        atomic_write(&pkg_root.join("build-results.json"), &txt);
    }
}

/// Write `txt` to `path` atomically (temp + rename) so a concurrent reader (the snapshot's deb_status
/// scan, ~1 Hz) never sees a torn file.
fn atomic_write(path: &Path, txt: &str) {
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, txt).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

// POSIX signal numbers (Linux). Builders run as their own process-group leaders so these reach the
// whole tree (python -> fontmake -> ninja/ttx). Freeze=SIGSTOP, thaw=SIGCONT, reap-on-stop=SIGKILL.
const SIGKILL: i32 = 9;
const SIGCONT: i32 = 18;
const SIGSTOP: i32 = 19;

/// Send `sig` to a whole process group (negative pid) — catches the builder's descendants, not just
/// the direct child. A pgid <= 1 is never signalled (would hit pid 1 / every process).
pub(crate) fn signal_group(pgid: i32, sig: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    if pgid > 1 {
        unsafe {
            kill(-pgid, sig);
        }
    }
}

/// The (orchestrator, compiler) attempt chain for one family. Pure — fully determined by the
/// backend setting, the orchestrator preference, and which tool binaries resolved — so every
/// combination is unit-tested. builder3 embeds fontc as a library: a builder3 attempt needs no
/// fontc binary and can never run fontmake.
///
///   auto/auto      → builder3+fontc → builder2+fontc → builder2+fontmake
///   fontc          → builder3+fontc → builder2+fontc            (no fontmake rescue)
///   fontmake       → builder2+fontmake                          (builder3 can't run fontmake)
///   orchestrator=builder3 → builder3-only (an explicit "no Python fallback" run)
///   orchestrator=builder2 → the pre-builder3 behavior
///
/// An empty chain means "nothing can run this backend" — the caller fails the family with a
/// clear message instead of guessing.
pub fn attempt_chain(
    backend: &str,
    orchestrator: &str,
    have_fontc: bool,
    have_builder3: bool,
) -> Vec<(&'static str, &'static str)> {
    let b3 = have_builder3 && orchestrator != "builder2";
    let mut chain: Vec<(&'static str, &'static str)> = Vec::new();
    match backend {
        "fontmake" => {
            if orchestrator != "builder3" {
                chain.push(("builder2", "fontmake"));
            }
        }
        "fontc" => {
            if b3 {
                chain.push(("builder3", "fontc"));
            }
            if orchestrator != "builder3" && have_fontc {
                chain.push(("builder2", "fontc"));
            }
        }
        // auto (and anything unrecognized): the full graceful ladder
        _ => {
            if b3 {
                chain.push(("builder3", "fontc"));
            }
            if orchestrator != "builder3" {
                if have_fontc {
                    chain.push(("builder2", "fontc"));
                }
                chain.push(("builder2", "fontmake"));
            }
        }
    }
    chain
}

/// A result's quality rung on the Rust-migration ladder: builder3 (2, Python-free) >
/// builder2+fontc (1) > fontmake / legacy records (0). Used both to grade prior results
/// (auto-upgrade eligibility) and to filter an upgrade's attempt chain to strictly-better rungs.
pub fn result_rung(builder: &str, backend: &str) -> u8 {
    if builder == "builder3" {
        2
    } else if backend == "fontc" {
        1
    } else {
        0
    }
}

/// Move a family's current (flat) output fonts aside before an upgrade attempt, into
/// `<build-dir>/variants/<slug>/<builder>-<backend>/`. A SUCCESSFUL upgrade leaves them there —
/// every rung's binaries are kept so they can be compared later (the M3 axis) — while a declined
/// upgrade moves them straight back via `unstash_outputs`. The variants tree is deliberately
/// OUTSIDE `out/`: everything that scans out/ (packaging, QA, comparisons) must keep seeing
/// exactly one canonical set of fonts per family.
fn stash_variant_outputs(build_dir: &Path, out_dir: &Path, logname: &str, prior: &Res) -> Option<PathBuf> {
    let builder = if prior.builder.is_empty() { "builder2" } else { prior.builder.as_str() };
    let backend = if prior.backend.is_empty() { "unknown" } else { prior.backend.as_str() };
    let dest = build_dir.join("variants").join(logname).join(format!("{}-{}", builder, backend));
    let mut moved = false;
    if let Ok(entries) = std::fs::read_dir(out_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file() && matches!(p.extension().and_then(|x| x.to_str()), Some("ttf") | Some("otf")) {
                if !moved {
                    if std::fs::create_dir_all(&dest).is_err() {
                        return None; // can't preserve → leave everything in place
                    }
                    moved = true;
                }
                let _ = std::fs::rename(&p, dest.join(e.file_name()));
            }
        }
    }
    if moved { Some(dest) } else { None }
}

/// Remove a tree file-by-file, accumulating freed bytes into `freed` and reporting after every
/// file via `cb` (the caller throttles publication) — so the reset tab's progress bar moves while
/// a multi-GB portion (venvs!) is being deleted, instead of jumping 100% at the end.
fn remove_tree_progress(dir: &Path, freed: &mut u64, cb: &mut dyn FnMut(u64)) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                remove_tree_progress(&p, freed, cb);
            } else {
                if let Ok(m) = e.metadata() {
                    *freed += m.len();
                }
                let _ = std::fs::remove_file(&p);
                cb(*freed);
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Reset-tab: the out/ font dirs to delete for one COMPILER (`fontc`/`fontmake`) — a plain
/// family's whole out/<logname>, a both-mode family's matching-compiler subdir — skipping any
/// family currently building. Pure (returns the path list + #skipped) so it's unit-tested. The path
/// selection is the same as --discard-fonts, but the caller then CASCADES — see spawn_reset_portion /
/// families_to_requeue — re-queueing every family it left with no fonts (the result reset lives there,
/// not here), so this is no longer result-preserving like --discard-fonts.
fn select_font_targets(out_root: &Path, built: &[(String, String)], compiler: &str,
                       building: &HashSet<String>) -> (Vec<PathBuf>, usize) {
    let mut targets = Vec::new();
    let mut skipped = 0;
    for (logname, backend) in built {
        if building.contains(logname) { skipped += 1; continue; }
        if backend == compiler { targets.push(out_root.join(logname)); }
        else if backend == "both" { targets.push(out_root.join(logname).join(compiler)); }
    }
    (targets, skipped)
}

/// After a `fonts-<compiler>` deletion, decide which built families are left with NO fonts (fully
/// un-built) — the cascade re-queues exactly these so the progress bar regresses. `built` is
/// (slug, logname, backend) for the families that were built; `building` are lognames whose fonts
/// were skipped (still building) and so are never reset. A family whose backend IS this compiler had
/// its whole out/<logname> deleted → fully un-built. A `both`-backend family had only one half
/// deleted, so it's fully un-built only when the OTHER compiler's half is also gone, decided by
/// `other_half_present(logname)` (queried AFTER the delete). Other backends aren't part of this
/// portion. Returns (slug, logname) pairs.
fn families_to_requeue(
    built: &[(String, String, String)],
    compiler: &str,
    building: &HashSet<String>,
    mut other_half_present: impl FnMut(&str) -> bool,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (slug, logname, backend) in built {
        if building.contains(logname) { continue; }
        let fully_unbuilt = if backend == compiler {
            true
        } else if backend == "both" {
            !other_half_present(logname)
        } else {
            false
        };
        if fully_unbuilt { out.push((slug.clone(), logname.clone())); }
    }
    out
}

/// Undo `stash_variant_outputs` after a declined upgrade: the kept binaries go back to being the
/// family's canonical output, and the emptied variant dir is removed.
fn unstash_outputs(stash: &Path, out_dir: &Path) {
    let _ = std::fs::create_dir_all(out_dir);
    if let Ok(entries) = std::fs::read_dir(stash) {
        for e in entries.flatten() {
            let _ = std::fs::rename(e.path(), out_dir.join(e.file_name()));
        }
    }
    let _ = std::fs::remove_dir(stash); // only if now empty
    if let Some(parent) = stash.parent() {
        let _ = std::fs::remove_dir(parent); // remove variants/<slug> too when empty
    }
}

/// Per-build CPU budget: with `jobs` builds in flight, each gets ~total/jobs CPUs. The children
/// are themselves heavily parallel (builder3 defaults to ALL cores, fontc is rayon-wide, ninja
/// runs cpus+2 edges, fontmake forks) — without a budget, jobs × cores produced the triple-digit
/// load averages Simon reported.
pub fn inner_jobs(total_cpus: usize, jobs: usize) -> usize {
    (total_cpus / jobs.max(1)).max(1)
}

/// Worker `w`'s CPU range for `taskset -c` ("4-7") — disjoint across workers when jobs×inner ≤
/// total, so each build is HARD-confined to its slice and a child's internal over-parallelism
/// (ninja, python multiprocessing, anything) physically cannot swamp the machine. Affinity is
/// inherited by every descendant process.
pub fn cpu_slice(worker: usize, inner: usize, total: usize) -> String {
    let total = total.max(1);
    let start = (worker * inner) % total;
    let end = (start + inner - 1).min(total - 1);
    if start == end { format!("{}", start) } else { format!("{}-{}", start, end) }
}

fn taskset_available() -> bool {
    static AVAIL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAIL.get_or_init(|| {
        std::process::Command::new("taskset").arg("-V")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false)
    })
}

/// The argv for one builder attempt — factored out of run_builder so the exact command shapes
/// are unit-testable. Returns (program, args). `inner` caps the attempt's internal parallelism
/// (builder3's own job pool; the slice + RAYON env cover the rest).
fn builder_command(
    builder: &str,
    backend: &str,
    python: &str,
    config_path: &Path,
    fontc_bin: Option<&str>,
    builder3_bin: Option<&str>,
    inner: usize,
) -> (String, Vec<String>) {
    if builder == "builder3" {
        let b3 = builder3_bin.unwrap_or("gftools-builder");
        return (b3.to_string(), vec![
            config_path.to_string_lossy().into_owned(),
            "--jobs".into(), inner.to_string(),
        ]);
    }
    let mut args = vec!["-m".to_string(), "gftools.builder".to_string(), config_path.to_string_lossy().into_owned()];
    if backend == "fontc" {
        if let Some(fc) = fontc_bin {
            args.push("--experimental-fontc".into());
            args.push(fc.to_string());
        }
    }
    (python.to_string(), args)
}

/// Run one (orchestrator, compiler) build attempt. builder=="builder3" invokes the Rust-native
/// builder3 binary directly (no Python in the loop); else `python -m gftools.builder <config>`
/// (with --experimental-fontc for the fontc backend).
fn run_builder(
    python: &str,
    config_path: &Path,
    work: &Path,
    log_path: &Path,
    timeout: Option<u64>,
    builder: &str,
    backend: &str,
    fontc_bin: Option<&str>,
    builder3_bin: Option<&str>,
    inner: usize,
    cpu_slice: Option<&str>,
    slug: &str,
    running: &Mutex<RunReg>,
    frozen: &AtomicBool,
    job_limit: &AtomicUsize,
) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let logf = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("open log: {}", e))?;
    let logf2 = logf.try_clone().map_err(|e| format!("clone log fd: {}", e))?;

    // run_builder sets current_dir(work) below, so a RELATIVE venv python (from a relative --build-dir,
    // or the default relative --data-dir) would resolve against work/ — breaking both the spawn AND the
    // venv-bin-on-PATH logic (gftools.builder shells out to `fontmake` by name, so the venv bin MUST be
    // on PATH). Resolve it to an absolute path once, up front.
    let python_owned = {
        let p = Path::new(python);
        if p.is_absolute() {
            None
        } else {
            Some(
                std::env::current_dir()
                    .map(|c| c.join(p).to_string_lossy().into_owned())
                    .unwrap_or_else(|_| python.to_string()),
            )
        }
    };
    let python: &str = python_owned.as_deref().unwrap_or(python);

    // We're going to change into the work directory and the config path is under
    // that, so we should relativize it.
    let config_path = if let Ok(rel) = config_path.strip_prefix(work) {
        rel
    } else {
        config_path
    };

    let (program, args) = builder_command(builder, backend, python, config_path, fontc_bin, builder3_bin, inner);
    // Confine the whole child tree to this worker's CPU slice (taskset affinity is inherited by
    // every descendant — ninja, fontmake, python multiprocessing), so jobs × per-child
    // parallelism can never exceed the machine. Skipped when taskset is absent (e.g. macOS);
    // the RAYON/--jobs caps below still apply there.
    let mut cmd;
    match cpu_slice {
        Some(slice) if taskset_available() => {
            cmd = Command::new("taskset");
            cmd.arg("-c").arg(slice).arg(&program).args(&args);
        }
        _ => {
            cmd = Command::new(&program);
            cmd.args(&args);
        }
    }
    // fontc (standalone or embedded in builder3) sizes its rayon pool from this
    cmd.env("RAYON_NUM_THREADS", inner.to_string());
    let orch = if builder == "builder3" { "gftools-builder3" } else { "gftools.builder" };
    log_line(log_path, &format!("===== {} (backend={}, inner_jobs={}{}) =====", orch, backend, inner,
        cpu_slice.map(|s| format!(", cpus {}", s)).unwrap_or_default()));
    // gftools.builder shells out to fontmake / ninja / gftools / ttfautohint BY NAME, so the chosen
    // interpreter's bin/ MUST be on PATH (running venv/bin/python does not by itself activate the
    // venv). Use the venv bin = the python's parent dir WITHOUT resolving symlinks (canonicalize would
    // follow venv/bin/python → the system /usr/bin and miss fontmake). python is absolute (resolved above).
    // This applies to builder3 children too, deliberately: harmless when builder3 needs nothing from
    // the venv, and correct if it ever shells out to a tool by name (e.g. ttfautohint).
    let bindir = {
        let p = Path::new(python);
        if p.is_absolute() { p.parent().map(|d| d.to_path_buf()) } else { None }
    };
    if let Some(b) = bindir {
        let path = match std::env::var("PATH") {
            Ok(p) => format!("{}:{}", b.display(), p),
            Err(_) => b.display().to_string(),
        };
        cmd.env("PATH", path);
    }
    cmd.current_dir(work)
        .env("SOURCE_DATE_EPOCH", "0")
        // Use protobuf's pure-Python runtime so an OLD pinned toolchain (e.g. gftools 0.7.x / fontmake
        // 2.x) whose generated _pb2.py was built against protobuf<=3.20 still loads under the modern
        // protobuf installed in the venv (protobuf 4+ refuses the old C-descriptor codegen with
        // "Descriptors cannot be created directly"). Downgrading protobuf isn't an option — 3.20 has no
        // cp313 wheel. This is protobuf's own documented remedy; same wire format, identical output,
        // negligibly slower (gftools only parses small METADATA.pb / config files).
        .env("PROTOCOL_BUFFERS_PYTHON_IMPLEMENTATION", "python")
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(logf2));
    // own process group so a freeze/kill reaches fontmake/ninja/ttx descendants, not just python
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| format!("could not launch builder: {}", e))?;
    let pgid = child.id() as i32; // == pid, since the child leads its own process group
    // Register and (if a pause already landed) self-freeze ATOMICALLY against the signal sweeps. Holding
    // `running` across the frozen-check-and-SIGSTOP means apply_live's SIGSTOP/SIGCONT sweep (which also
    // locks `running`) cannot interleave between our check and our stop — so a pause→resume double-tap
    // can never leave this build frozen-but-never-thawed (the resume sweep runs strictly before the
    // insert, so we read frozen=false, or strictly after our SIGSTOP, so it SIGCONTs the stopped group).
    {
        let mut g = running.lock().unwrap();
        // start frozen if globally paused, OR if `jobs` builds are already actively compiling (this one
        // is the excess — the gate let it through during a light checkout phase, or a live jobs cut just
        // landed). The regulator thaws it later when a slot frees.
        let start_frozen = frozen.load(Ordering::Relaxed)
            || g.unfrozen() >= job_limit.load(Ordering::Relaxed);
        g.insert(pgid, start_frozen, slug.to_string());
        if start_frozen {
            signal_group(pgid, SIGSTOP);
        }
    }
    // Deregister on every exit path (incl. unwind). The reap sites below also remove pgid the moment the
    // child exits, shrinking the (already sub-ms) pid-reuse window in which a stale pgid could be
    // signalled; this guard is the panic/early-return backstop (remove of an absent key is harmless).
    struct Dereg<'a>(&'a Mutex<RunReg>, i32);
    impl Drop for Dereg<'_> {
        fn drop(&mut self) {
            if let Ok(mut s) = self.0.lock() {
                s.remove(self.1);
            }
        }
    }
    let _dereg = Dereg(running, pgid);

    // timeout poll. Frozen (paused) time must NOT count toward the deadline, else a long read-the-data
    // pause would spuriously time the build out.
    if let Some(t) = timeout {
        let mut deadline = std::time::Instant::now() + Duration::from_secs(t);
        loop {
            // ALWAYS reap first: a child that exited — including one SIGKILLed while frozen (e.g. by
            // "delete everything") — must be observed promptly so the worker returns and the drain
            // completes. (Previously the frozen branch `continue`d without try_wait, so a kill while
            // frozen was never reaped and the build hung.)
            match child.try_wait() {
                Ok(Some(st)) => {
                    running.lock().unwrap().remove(pgid); // exited+reaped → deregister before returning
                    return if st.success() { Ok(()) } else { Err(last_error_line(log_path)) };
                }
                Ok(None) => {} // still running — fall through to freeze / deadline handling
                Err(e) => return Err(format!("wait: {}", e)),
            }
            // frozen time — by a global pause OR by the job limit (this build's own freeze flag) — must
            // not count toward the deadline, else a long freeze would spuriously time the build out.
            if frozen.load(Ordering::Relaxed) || running.lock().unwrap().is_frozen(pgid) {
                thread::sleep(Duration::from_millis(300));
                deadline += Duration::from_millis(300); // don't count frozen time
                continue;
            }
            if std::time::Instant::now() >= deadline {
                signal_group(pgid, SIGKILL); // kill the whole group, then reap
                let _ = child.wait();
                running.lock().unwrap().remove(pgid);
                return Err(format!("timed out after {}s", t));
            }
            thread::sleep(Duration::from_millis(300));
        }
    } else {
        // no timeout: wait() blocks until exit; a frozen child simply keeps the worker parked here
        let st = child.wait().map_err(|e| format!("wait: {}", e))?;
        running.lock().unwrap().remove(pgid); // exited+reaped → deregister before returning
        if st.success() {
            Ok(())
        } else {
            Err(last_error_line(log_path))
        }
    }
}

fn last_error_line(log_path: &Path) -> String {
    let txt = std::fs::read_to_string(log_path).unwrap_or_default();
    let clip = |s: &str| -> String { s.chars().take(200).collect() };
    // A generic ninja/jobrunner wrapper line ("FAILED: [code=1] /tmp/…", "ninja: build stopped",
    // "Command failed:") carries NO cause — the real reason is the compiler/preprocessor error above
    // it. Skip wrappers so the recorded error (what the failure classifier sees) is actually diagnostic.
    let is_wrapper = |s: &str| {
        s.starts_with("FAILED:") || s.starts_with("ninja:") || s.starts_with("Command failed")
            || s.starts_with("Cleaning up") || s.starts_with("Done cleaning")
    };
    // pass 1: the actual fontc error message, or builder3's cyclic-graph report (most diagnostic)
    for ln in txt.lines().rev() {
        let s = ln.trim();
        if let Some(i) = s.find("fontc ERROR]") {
            let msg = s[i + "fontc ERROR]".len()..].trim();
            if !msg.is_empty() {
                return clip(msg);
            }
        }
        if s.contains("CYCLE DETECTED") || s.contains("Not a valid DAG") {
            return "cyclic dependency graph (subset inclusion) — not a valid DAG".into();
        }
    }
    // pass 2: a Python exception WITH a message ("fontTools…GlifLibError: …", "KeyError: …", …)
    for ln in txt.lines().rev() {
        let s = ln.trim();
        if !is_wrapper(s) && (s.contains("Error:") || s.contains("Exception:")) {
            return clip(s);
        }
    }
    // pass 3: the original keyword fallback, but never the bare ninja wrapper
    for ln in txt.lines().rev() {
        let s = ln.trim();
        if !s.is_empty()
            && !is_wrapper(s)
            && ["Error", "error", "Exception", "Traceback", "FAILED", "assert"]
                .iter()
                .any(|k| s.contains(k))
        {
            return clip(s);
        }
    }
    txt.lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(clip)
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
    // Tag each root: the per-family `work` tree is PRIVATE; `work.parent()/fonts` is SHARED across all
    // families building concurrently (an override config that runs from sources/ writes to ../fonts).
    // Matching shipped-named, fresh-mtime fonts are safe to collect from the shared dir, but a
    // NON-matching font there belongs to another family — never report it as THIS family's "extras"
    // (that produced misleading "output name mismatch — got [<other family>]" errors).
    let mut stack: Vec<(PathBuf, bool)> = vec![(work.to_path_buf(), false)];
    if let Some(parent) = work.parent() {
        let stray = parent.join("fonts");
        if stray.is_dir() {
            stack.push((stray, true));
        }
    }
    let mut fonts: Vec<(PathBuf, bool)> = Vec::new();
    while let Some((p, shared)) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push((path, shared));
                } else if let Some(ext) = path.extension() {
                    let ext = ext.to_string_lossy().to_lowercase();
                    if ext == "ttf" || ext == "otf" {
                        fonts.push((path, shared));
                    }
                }
            }
        }
    }
    fonts.sort();
    for (f, shared) in fonts {
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
            // only a font from the family's OWN work tree is a true "extra" (a real name mismatch);
            // a non-matching font in the shared ../fonts dir is another family's output — ignore it.
            if !shared && seen.insert(name.clone()) {
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

/// Compare fontc vs fontmake outputs (--backend both): identical / differ / "" if no comparable pair.
/// sha256-level (a table-tag diff is a future refinement — would need fontTools in the build python).
fn compare_backends(
    fontc: &BTreeMap<String, PathBuf>,
    fontmake: &BTreeMap<String, PathBuf>,
    shipped: &[String],
) -> String {
    let names: Vec<String> = if !shipped.is_empty() {
        shipped.to_vec()
    } else {
        fontc.keys().filter(|k| fontmake.contains_key(*k)).cloned().collect()
    };
    let mut any = false;
    let mut differ = false;
    for n in &names {
        if let (Some(a), Some(b)) = (fontc.get(n), fontmake.get(n)) {
            any = true;
            if sha256_file(a) != sha256_file(b) {
                differ = true;
            }
        }
    }
    if !any {
        String::new()
    } else if differ {
        "differ".into()
    } else {
        "identical".into()
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
mod jobs_freeze_tests {
    use super::*;
    fn reg(items: &[(i32, bool)]) -> RunReg {
        let mut r = RunReg::default();
        for &(p, f) in items {
            r.insert(p, f, format!("ofl/t{}", p));
        }
        r
    }
    fn sorted(mut v: Vec<i32>) -> Vec<i32> {
        v.sort();
        v
    }

    #[test]
    fn lowering_jobs_freezes_the_newest_excess() {
        // 5 running unfrozen, pgids inserted oldest→newest; jobs→2 freezes the 3 NEWEST, keeps 2 oldest
        let mut r = reg(&[(1, false), (2, false), (3, false), (4, false), (5, false)]);
        let (freeze, thaw) = r.plan(false, 2);
        assert!(thaw.is_empty());
        assert_eq!(sorted(freeze), vec![3, 4, 5]);
        assert_eq!(r.unfrozen(), 2);
        assert!(!r.is_frozen(1) && !r.is_frozen(2)); // the builds closest to finishing keep running
        assert!(r.is_frozen(3) && r.is_frozen(4) && r.is_frozen(5));
    }

    #[test]
    fn freeing_a_slot_thaws_the_oldest_frozen_first() {
        // jobs=2; 2 unfrozen (1,2) + 3 frozen (3,4,5). One unfrozen finishes → a slot opens.
        let mut r = reg(&[(1, false), (2, false), (3, true), (4, true), (5, true)]);
        r.remove(1);
        let (freeze, thaw) = r.plan(false, 2);
        assert!(freeze.is_empty());
        assert_eq!(thaw, vec![3]); // oldest frozen resumes (drain in-progress before new)
        assert_eq!(r.unfrozen(), 2);
    }

    #[test]
    fn raising_jobs_thaws_multiple_oldest_first() {
        let mut r = reg(&[(1, false), (2, false), (3, true), (4, true), (5, true)]); // was jobs=2
        let (freeze, thaw) = r.plan(false, 4); // raise to 4 → thaw the 2 oldest frozen
        assert!(freeze.is_empty());
        assert_eq!(thaw, vec![3, 4]);
        assert_eq!(r.unfrozen(), 4);
        assert!(r.is_frozen(5));
    }

    #[test]
    fn global_pause_freezes_all_then_resume_thaws_to_jobs() {
        let mut r = reg(&[(1, false), (2, false), (3, false)]);
        let (freeze, thaw) = r.plan(true, 2); // paused → freeze ALL, ignoring jobs
        assert!(thaw.is_empty());
        assert_eq!(sorted(freeze), vec![1, 2, 3]);
        assert_eq!(r.unfrozen(), 0);
        let (freeze2, thaw2) = r.plan(false, 2); // resume at jobs=2 → thaw the 2 oldest
        assert!(freeze2.is_empty());
        assert_eq!(thaw2, vec![1, 2]);
        assert_eq!(r.unfrozen(), 2);
    }

    #[test]
    fn jobs_zero_drains_thaws_frozen_and_freezes_nothing() {
        // jobs=0 is DRAIN, not pause: every in-flight build (running OR previously frozen) must run to
        // completion; the worker ready-gate blocks NEW builds. So plan(false,0) freezes nothing and thaws
        // ALL frozen — never leaving a build SIGSTOPped forever.
        let mut r = reg(&[(1, false), (2, false), (3, true), (4, true)]); // 2 running, 2 frozen
        let (freeze, thaw) = r.plan(false, 0);
        assert!(freeze.is_empty(), "drain must not freeze in-flight builds");
        assert_eq!(sorted(thaw), vec![3, 4], "drain thaws every frozen build so it can finish");
        assert_eq!(r.unfrozen(), 4);
        // a second pass is a no-op (all already unfrozen)
        let (f2, t2) = r.plan(false, 0);
        assert!(f2.is_empty() && t2.is_empty());
        // paused still wins over jobs=0 (freeze all), and resume at 0 drains again
        let (fp, _) = r.plan(true, 0);
        assert_eq!(sorted(fp), vec![1, 2, 3, 4]);
        let (_, tr) = r.plan(false, 0);
        assert_eq!(sorted(tr), vec![1, 2, 3, 4]);
    }

    #[test]
    fn already_at_target_is_a_noop() {
        let mut r = reg(&[(1, false), (2, false), (3, true)]);
        let (freeze, thaw) = r.plan(false, 2);
        assert!(freeze.is_empty() && thaw.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every (backend × fontc × builder3 × orchestrator) combination of the attempt chain —
    /// the "always prefer builder3, degrade gracefully" contract.
    #[test]
    fn attempt_chain_prefers_builder3_and_degrades() {
        // auto/auto, everything available: the full ladder
        assert_eq!(
            attempt_chain("auto", "auto", true, true),
            vec![("builder3", "fontc"), ("builder2", "fontc"), ("builder2", "fontmake")]
        );
        // builder3 unavailable → the pre-builder3 behavior
        assert_eq!(
            attempt_chain("auto", "auto", true, false),
            vec![("builder2", "fontc"), ("builder2", "fontmake")]
        );
        // no fontc binary, builder3 available: builder3 needs NO fontc binary (in-process)
        assert_eq!(
            attempt_chain("auto", "auto", false, true),
            vec![("builder3", "fontc"), ("builder2", "fontmake")]
        );
        // nothing available: fontmake-only (the guaranteed floor)
        assert_eq!(attempt_chain("auto", "auto", false, false), vec![("builder2", "fontmake")]);
    }

    #[test]
    fn attempt_chain_respects_explicit_backend() {
        // --backend fontc: never rescued by fontmake
        assert_eq!(
            attempt_chain("fontc", "auto", true, true),
            vec![("builder3", "fontc"), ("builder2", "fontc")]
        );
        assert_eq!(attempt_chain("fontc", "auto", true, false), vec![("builder2", "fontc")]);
        assert_eq!(attempt_chain("fontc", "auto", false, true), vec![("builder3", "fontc")]);
        // --backend fontc with no fontc anywhere: empty chain = a clear failure upstream
        assert!(attempt_chain("fontc", "auto", false, false).is_empty());
        // --backend fontmake: builder2 only — builder3 cannot run fontmake
        assert_eq!(attempt_chain("fontmake", "auto", true, true), vec![("builder2", "fontmake")]);
    }

    #[test]
    fn attempt_chain_respects_orchestrator_override() {
        // --orchestrator builder2: builder3 fully out, even when available
        assert_eq!(
            attempt_chain("auto", "builder2", true, true),
            vec![("builder2", "fontc"), ("builder2", "fontmake")]
        );
        // --orchestrator builder3: an explicit no-Python-fallback run
        assert_eq!(attempt_chain("auto", "builder3", true, true), vec![("builder3", "fontc")]);
        assert_eq!(attempt_chain("fontmake", "builder3", true, true), Vec::<(&str, &str)>::new());
        // builder3 forced but unavailable: empty (fail loudly, don't silently use Python)
        assert!(attempt_chain("auto", "builder3", true, false).is_empty());
    }

    #[test]
    fn result_rung_orders_the_migration_ladder() {
        assert_eq!(result_rung("builder3", "fontc"), 2);
        assert_eq!(result_rung("builder2", "fontc"), 1);
        assert_eq!(result_rung("builder2", "fontmake"), 0);
        assert_eq!(result_rung("", ""), 0); // legacy pre-M0 record == fontmake-era floor
        // the upgrade filter contract: only strictly-better rungs are attempted
        let fontmake_floor = result_rung("builder2", "fontmake");
        let chain = attempt_chain("auto", "auto", true, true);
        let upgrades: Vec<_> = chain.into_iter().filter(|(b, c)| result_rung(b, c) > fontmake_floor).collect();
        assert_eq!(upgrades, vec![("builder3", "fontc"), ("builder2", "fontc")]);
        // a builder2+fontc success only has builder3 left above it
        let fc_floor = result_rung("builder2", "fontc");
        let chain = attempt_chain("auto", "auto", true, true);
        let upgrades: Vec<_> = chain.into_iter().filter(|(b, c)| result_rung(b, c) > fc_floor).collect();
        assert_eq!(upgrades, vec![("builder3", "fontc")]);
        // a builder3 result has nowhere to go
        let chain = attempt_chain("auto", "auto", true, true);
        assert!(chain.into_iter().filter(|(b, c)| result_rung(b, c) > 2).next().is_none());
    }

    #[test]
    fn run_sig_tracks_pins_orchestrator_and_capabilities() {
        use crate::toolchain::run_sig;
        let s = run_sig("auto", true, true);
        assert!(s.contains(&crate::toolchain::BUILDER3_REV[..10]));
        assert!(s.contains(crate::toolchain::FONTC_VERSION));
        // preference change re-arms upgrades
        assert_ne!(s, run_sig("builder2", true, true));
        // a DEGRADED run (builder3 unavailable) stamps differently from a fully-capable one, so
        // results built while a tool was missing re-arm automatically once it provisions
        assert_ne!(run_sig("auto", true, false), run_sig("auto", true, true));
        assert_ne!(run_sig("auto", false, true), run_sig("auto", true, true));
    }

    #[test]
    fn stash_and_unstash_preserve_the_prior_binaries() {
        let root = std::env::temp_dir().join(format!("_stash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let out = root.join("out").join("ofl__abel");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("Abel-Regular.ttf"), b"FONTMAKE").unwrap();
        std::fs::write(out.join("build.log"), b"not a font").unwrap();
        let prior = Res { builder: "builder2".into(), backend: "fontmake".into(), ..Default::default() };
        // stash: fonts move to variants/<slug>/builder2-fontmake/, non-fonts stay
        let stash = stash_variant_outputs(&root, &out, "ofl__abel", &prior).expect("stashed");
        assert!(stash.ends_with("variants/ofl__abel/builder2-fontmake"));
        assert!(stash.join("Abel-Regular.ttf").is_file());
        assert!(!out.join("Abel-Regular.ttf").exists());
        assert!(out.join("build.log").is_file());
        // declined upgrade: unstash moves them back and removes the empty variant dirs
        unstash_outputs(&stash, &out);
        assert_eq!(std::fs::read(out.join("Abel-Regular.ttf")).unwrap(), b"FONTMAKE");
        assert!(!stash.exists());
        // empty out dir → nothing to stash → None (an upgrade with --discard-fonts priors)
        let out2 = root.join("out").join("ofl__empty");
        std::fs::create_dir_all(&out2).unwrap();
        assert!(stash_variant_outputs(&root, &out2, "ofl__empty", &prior).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_built_fonts_targets_only_the_matching_compiler() {
        let root = std::env::temp_dir().join(format!("_reset_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let out = root.join("out");
        for d in ["ofl__a", "ofl__b", "ofl__c/fontc", "ofl__c/fontmake"] {
            std::fs::create_dir_all(out.join(d)).unwrap();
        }
        std::fs::write(out.join("ofl__a/A.ttf"), b"fontc-built").unwrap();
        std::fs::write(out.join("ofl__b/B.ttf"), b"fontmake-built").unwrap();
        std::fs::write(out.join("ofl__c/fontc/C.ttf"), b"c1").unwrap();
        std::fs::write(out.join("ofl__c/fontmake/C.ttf"), b"c2").unwrap();
        let built = vec![
            ("ofl__a".to_string(), "fontc".to_string()),
            ("ofl__b".to_string(), "fontmake".to_string()),
            ("ofl__c".to_string(), "both".to_string()),
        ];
        // pick the fontc targets, with ofl__b currently BUILDING (must be skipped if it matched)
        let building: std::collections::HashSet<String> = std::iter::once("ofl__never".to_string()).collect();
        let (targets, skipped) = select_font_targets(&out, &built, "fontc", &building);
        assert_eq!(skipped, 0);
        assert_eq!(targets.len(), 2, "fontc plain dir + both-mode fontc subdir");
        for t in &targets { if t.is_dir() { let _ = std::fs::remove_dir_all(t); } }
        assert!(!out.join("ofl__a").exists(), "fontc-built family deleted");
        assert!(out.join("ofl__b/B.ttf").is_file(), "fontmake-built family untouched");
        assert!(!out.join("ofl__c/fontc").exists(), "both-mode fontc subdir deleted");
        assert!(out.join("ofl__c/fontmake/C.ttf").is_file(), "both-mode fontmake subdir kept");
        // a currently-building family is skipped, not deleted
        let busy: std::collections::HashSet<String> = std::iter::once("ofl__a".to_string()).collect();
        let (t2, s2) = select_font_targets(&out, &built, "fontc", &busy);
        assert_eq!(s2, 1);
        assert!(!t2.iter().any(|p| p.ends_with("ofl__a")), "building family not targeted");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cascade_requeues_only_fully_unbuilt_families() {
        // (slug, logname, backend): a fontc-only, a fontmake-only, a 'both', and one currently building
        let built = vec![
            ("ofl/a".to_string(), "ofl__a".to_string(), "fontc".to_string()),
            ("ofl/b".to_string(), "ofl__b".to_string(), "fontmake".to_string()),
            ("ofl/c".to_string(), "ofl__c".to_string(), "both".to_string()),
            ("ofl/d".to_string(), "ofl__d".to_string(), "fontc".to_string()),
        ];
        let building: HashSet<String> = std::iter::once("ofl__d".to_string()).collect();

        // delete fontc fonts: 'a' (fontc) is fully un-built; 'b' (fontmake) untouched; 'd' is building
        // → skipped; 'c' (both) keeps its fontmake half (present) so it's NOT yet fully un-built.
        let r = families_to_requeue(&built, "fontc", &building, |logname| logname == "ofl__c");
        assert_eq!(r, vec![("ofl/a".to_string(), "ofl__a".to_string())]);

        // now delete fontmake fonts: 'b' is fully un-built, and 'c' (both) has lost its fontc half too
        // (other_half_present=false for everyone) → 'c' is now fully un-built and re-queued as well.
        let r2 = families_to_requeue(&built, "fontmake", &HashSet::new(), |_| false);
        assert_eq!(r2, vec![
            ("ofl/b".to_string(), "ofl__b".to_string()),
            ("ofl/c".to_string(), "ofl__c".to_string()),
        ]);
    }

    #[test]
    fn builder_command_shapes() {
        let cfgp = Path::new("sources/config.yaml");
        // builder3: the binary, the config, and its own job cap (no Python anywhere)
        let (prog, args) = builder_command("builder3", "fontc", "/v/bin/python", cfgp, Some("/t/fontc"), Some("/t/gftools-builder"), 4);
        assert_eq!(prog, "/t/gftools-builder");
        assert_eq!(args, vec!["sources/config.yaml", "--jobs", "4"]);
        // builder2 + fontc: gftools.builder with --experimental-fontc
        let (prog, args) = builder_command("builder2", "fontc", "/v/bin/python", cfgp, Some("/t/fontc"), None, 4);
        assert_eq!(prog, "/v/bin/python");
        assert_eq!(args, vec!["-m", "gftools.builder", "sources/config.yaml", "--experimental-fontc", "/t/fontc"]);
        // builder2 + fontmake: plain gftools.builder
        let (prog, args) = builder_command("builder2", "fontmake", "/v/bin/python", cfgp, Some("/t/fontc"), None, 4);
        assert_eq!(prog, "/v/bin/python");
        assert_eq!(args, vec!["-m", "gftools.builder", "sources/config.yaml"]);
        // the CPU budget: jobs*inner ≈ cpus, never zero
        assert_eq!(inner_jobs(32, 10), 3);
        assert_eq!(inner_jobs(8, 8), 1);
        assert_eq!(inner_jobs(4, 16), 1);  // oversubscribed jobs: floor of 1
        assert_eq!(inner_jobs(64, 1), 64); // a single build may use the whole machine
        // disjoint slices while jobs*inner <= total; wraps (never panics) past it
        assert_eq!(cpu_slice(0, 4, 32), "0-3");
        assert_eq!(cpu_slice(7, 4, 32), "28-31");
        assert_eq!(cpu_slice(8, 4, 32), "0-3"); // wraps
        assert_eq!(cpu_slice(0, 1, 8), "0");
        assert_eq!(cpu_slice(3, 3, 8), "1-3"); // partial overlap when not evenly divisible
    }

    #[test]
    fn mirror_path_maps_urls() {
        let ar = Path::new("/arch");
        assert_eq!(mirror_path(ar, "https://github.com/googlefonts/foo"), Path::new("/arch/googlefonts/foo.git"));
        assert_eq!(mirror_path(ar, "https://github.com/googlefonts/foo.git"), Path::new("/arch/googlefonts/foo.git"));
        assert_eq!(mirror_path(ar, "git@github.com:owner/bar.git"), Path::new("/arch/owner/bar.git"));
    }

    #[test]
    fn config_signature_tracks_only_marked_overrides() {
        // a natural upstream config (no marker) with no build_rules entry is never tracked -> never rebuilt
        assert_eq!(config_signature("sources:\n  - Foo.glyphs\n", None), "");
        // but a build_rules-only fix (no override config, yet a pre-build entry) IS tracked
        let pre = vec!["touch sources/family.fea".to_string()];
        assert!(!config_signature("sources:\n  - Foo.glyphs\n", Some(&pre)).is_empty());
        // a gflib-build override changes signature when its text changes...
        let a = config_signature("# gflib-build override\nrecipe:\n  x:\n    - source: a\n", None);
        let b = config_signature("# gflib-build override\nrecipe:\n  x:\n    - source: b\n", None);
        assert!(!a.is_empty() && a != b);
        // ...and when only its build_rules (staging) entry changes
        let rule1 = vec!["cp a sources/a".to_string()];
        let rule2 = vec!["cp b sources/b".to_string()];
        let txt = "# gflib-build override\nrecipe:\n  x:\n    - source: a\n";
        assert_ne!(config_signature(txt, Some(&rule1)), config_signature(txt, Some(&rule2)));
        // identical inputs are stable (so a still-failing rebuild doesn't loop)
        assert_eq!(config_signature(txt, Some(&rule1)), config_signature(txt, Some(&rule1)));
    }

    #[test]
    fn packaging_io_roundtrips_and_counts() {
        let dir = std::env::temp_dir().join(format!("gflib-debio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // build-results.json: round-trips per-slug and recomputes built/failed
        let mut results = serde_json::Map::new();
        results.insert("ofl/a".into(), serde_json::json!({"built": true, "package": "fonts-gf-a"}));
        results.insert("ofl/b".into(), serde_json::json!({"built": false, "error": "x"}));
        write_deb_results(&dir, &results);
        let back = read_deb_results(&dir);
        assert_eq!(back.len(), 2);
        assert_eq!(back["ofl/a"]["built"], serde_json::json!(true));
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("build-results.json")).unwrap()).unwrap();
        assert_eq!(doc["built"], serde_json::json!(1));
        assert_eq!(doc["failed"], serde_json::json!(1));
        // index.json: keyed by slug, round-trips
        let mut index = std::collections::BTreeMap::new();
        index.insert("ofl/a".to_string(), serde_json::json!({"slug":"ofl/a","package":"fonts-gf-a"}));
        write_pkg_index(&dir, &index);
        let bi = read_pkg_index(&dir);
        assert_eq!(bi.len(), 1);
        assert_eq!(bi["ofl/a"]["package"], serde_json::json!("fonts-gf-a"));
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn collect_outputs_ignores_other_families_fonts_in_shared_stray_dir() {
        // the akshar→"got [AbhayaLibre…]" bug: a non-matching font in the SHARED ../fonts dir belongs
        // to another family building concurrently and must NOT be reported as our "extras".
        let root = std::env::temp_dir().join(format!("_stray2_{}", std::process::id()));
        let work = root.join("work").join("ofl__akshar");
        let stray = root.join("work").join("fonts"); // work.parent()/fonts — shared
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("AbhayaLibreLatin-Bold.otf"), b"OTHERFAM").unwrap();
        std::fs::write(work.join("Akshar-Wrong.ttf"), b"OURS").unwrap(); // our own real mismatch
        let out = root.join("out");
        let since = crate::util::now() - 1.0;
        let (_t, found, extras) = collect_outputs(&work, &out, &["Akshar[wght].ttf".to_string()], since);
        assert!(found.is_empty(), "neither font matches the shipped name");
        assert!(extras.contains(&"Akshar-Wrong.ttf".to_string()), "our own mismatch IS a real extra");
        assert!(
            !extras.iter().any(|e| e.contains("AbhayaLibre")),
            "another family's shared-dir font must NOT be reported as our extra"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
