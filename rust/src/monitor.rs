//! Read-only monitoring. `MonitorState` reads a daemon's `status.json` and re-parses it only when
//! its mtime changes (the file is tens of KB and the UI polls a few times a second, so mtime-gating
//! keeps the dashboard snappy even on networked filesystems — same trick as the Python monitor).
//!
//! `Source` is the small abstraction every frontend renders against: either the live `Orchestrator`
//! (a real build) or a `MonitorState` (attached to someone else's daemon). Both expose `snapshot()`,
//! so the TUI and web UIs are identical whether you launched the build or are just watching it.

use crate::build::Orchestrator;
use crate::model::{ControlSet, Snapshot};
use crate::persist;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// A static `Source` for the first-run setup wizard: it has no build, just the initial config the
/// config tab edits. The TUI renders it in `pre_build` mode and returns the edited config to launch.
pub struct SetupState {
    config: BTreeMap<String, serde_json::Value>,
    build_dir: PathBuf,
}

impl SetupState {
    pub fn new(config: BTreeMap<String, serde_json::Value>, build_dir: PathBuf) -> Arc<Self> {
        Arc::new(SetupState { config, build_dir })
    }
}

impl Source for SetupState {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            config: self.config.clone(),
            phase: "config".into(),
            pre_build: true,
            daemon_alive: true,
            ..Default::default()
        }
    }
    fn build_dir(&self) -> PathBuf {
        self.build_dir.clone()
    }
    fn is_live(&self) -> bool {
        false
    }
    fn control(&self, _set: &ControlSet) {}
}

/// Something a frontend can render + send live controls to.
#[allow(dead_code)] // build_dir()/request_stop() are part of the stable Source API; not all callers use them yet
pub trait Source: Send + Sync {
    fn snapshot(&self) -> Snapshot;
    fn build_dir(&self) -> PathBuf;
    /// True for the live orchestrator (we own the build), false for a read-only monitor.
    fn is_live(&self) -> bool;
    /// Apply a control: live orchestrator applies in-process; a monitor writes control.json.
    fn control(&self, set: &ControlSet);
    /// Stop the build (live only); a monitor signals the daemon via SIGTERM elsewhere.
    fn request_stop(&self) {}
}

impl Source for Orchestrator {
    fn snapshot(&self) -> Snapshot {
        Orchestrator::snapshot(self)
    }
    fn build_dir(&self) -> PathBuf {
        self.cfg.build_dir.clone()
    }
    fn is_live(&self) -> bool {
        true
    }
    fn control(&self, _set: &ControlSet) {
        // for the live orchestrator we need &Arc<Self>; the in-process frontend instead calls
        // apply_live directly on the Arc. control() here writes control.json as a uniform fallback.
        persist::write_control(&self.cfg.build_dir, _set);
    }
    fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cond.notify_all();
    }
}

pub struct MonitorState {
    build_dir: PathBuf,
    cache: Mutex<MonCache>,
}

struct MonCache {
    snap: Snapshot,
    mtime: Option<u128>,
    fontspector: Option<crate::model::FontspectorView>, // QA aggregate, refreshed with the status reparse
    last_check: std::time::Instant,
}

impl MonitorState {
    pub fn new(build_dir: &Path) -> Arc<Self> {
        Arc::new(MonitorState {
            build_dir: build_dir.to_path_buf(),
            cache: Mutex::new(MonCache {
                snap: Snapshot::default(),
                mtime: None,
                fontspector: None,
                last_check: std::time::Instant::now() - std::time::Duration::from_secs(10),
            }),
        })
    }

    pub fn daemon_alive(&self) -> bool {
        persist::read_daemon_pid(&self.build_dir).is_some()
    }
}

impl Source for MonitorState {
    fn snapshot(&self) -> Snapshot {
        let mut c = self.cache.lock().unwrap();
        // throttle filesystem stat to ~4×/s
        if c.last_check.elapsed() < std::time::Duration::from_millis(200) {
            let mut s = c.snap.clone();
            s.daemon_alive = self.daemon_alive();
            s.fontspector = c.fontspector.clone();
            return s;
        }
        c.last_check = std::time::Instant::now();
        let mt = persist::status_mtime(&self.build_dir);
        if mt != c.mtime {
            if let Some(snap) = persist::read_status(&self.build_dir) {
                c.snap = snap;
                c.mtime = mt;
            }
        }
        // refresh the fontspector QA aggregate at the throttled cadence (it has its own mtime — a
        // --fontspector pass writes _summary.json without touching status.json)
        c.fontspector = persist::read_fontspector_summary(&self.build_dir);
        let mut s = c.snap.clone();
        s.daemon_alive = self.daemon_alive();
        s.fontspector = c.fontspector.clone();
        s
    }
    fn build_dir(&self) -> PathBuf {
        self.build_dir.clone()
    }
    fn is_live(&self) -> bool {
        false
    }
    fn control(&self, set: &ControlSet) {
        persist::write_control(&self.build_dir, set);
    }
}
