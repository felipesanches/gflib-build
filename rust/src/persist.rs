//! All on-disk persistence under <build-dir>, kept schema-compatible with the Python tool:
//!   status.json            live snapshot (atomic rename each ~1 s; read by any monitor)
//!   control.json           live-control channel a monitor bumps; the daemon polls + applies it
//!   state.json             resumable per-family status (built/failed/…) across runs
//!   failure-history.jsonl  append-only durable record of how families broke
//!   daemon.pid             PID of a detached build daemon (for attach/stop)

use crate::model::{Control, FailHist, FontspectorView, Res, Snapshot, StateFile};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn status_path(build_dir: &Path) -> PathBuf { build_dir.join("status.json") }
/// Directory holding fontspector QA results: per-family <slug__>.json + _summary.json (the aggregate).
pub fn fontspector_dir(build_dir: &Path) -> PathBuf { build_dir.join("fontspector") }
/// Directory holding diffenator3 vs-shipped results: per-family <slug__>.json + _summary.json.
pub fn diffenator3_dir(build_dir: &Path) -> PathBuf { build_dir.join("diffenator3") }

/// Read the fontspector aggregate (build_dir/fontspector/_summary.json) for the breakdown panels.
pub fn read_fontspector_summary(build_dir: &Path) -> Option<FontspectorView> {
    let txt = std::fs::read_to_string(fontspector_dir(build_dir).join("_summary.json")).ok()?;
    serde_json::from_str(&txt).ok()
}
pub fn control_path(build_dir: &Path) -> PathBuf { build_dir.join("control.json") }
pub fn state_path(build_dir: &Path) -> PathBuf { build_dir.join("state.json") }
pub fn fail_hist_path(build_dir: &Path) -> PathBuf { build_dir.join("failure-history.jsonl") }
pub fn pid_path(build_dir: &Path) -> PathBuf { build_dir.join("daemon.pid") }

/// Atomically write the snapshot to status.json (write tmp + rename, so a reader never sees a torn
/// file) — exactly the Python daemon's contract.
pub fn write_status(build_dir: &Path, snap: &Snapshot) {
    let _ = std::fs::create_dir_all(build_dir);
    let tmp = build_dir.join("status.json.tmp");
    if let Ok(txt) = serde_json::to_string(snap) {
        if std::fs::write(&tmp, txt).is_ok() {
            let _ = std::fs::rename(&tmp, status_path(build_dir));
        }
    }
}

/// Read + parse status.json (None if absent/unparseable).
pub fn read_status(build_dir: &Path) -> Option<Snapshot> {
    let txt = std::fs::read_to_string(status_path(build_dir)).ok()?;
    serde_json::from_str(&txt).ok()
}

/// mtime of status.json in whole-nanoseconds, for the monitor's mtime-gated re-parse.
pub fn status_mtime(build_dir: &Path) -> Option<u128> {
    let md = std::fs::metadata(status_path(build_dir)).ok()?;
    let mt = md.modified().ok()?;
    Some(mt.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// Read control.json (the live-control channel). None if absent.
pub fn read_control(build_dir: &Path) -> Option<Control> {
    let txt = std::fs::read_to_string(control_path(build_dir)).ok()?;
    serde_json::from_str(&txt).ok()
}

/// Bump control.json with a new set of live settings — what a monitor (TUI/web) calls to apply a
/// change to the running daemon. Increments `seq` so the daemon notices it changed.
pub fn write_control(build_dir: &Path, set: &crate::model::ControlSet) -> bool {
    let prev_seq = read_control(build_dir).map(|c| c.seq).unwrap_or(0);
    let ctl = Control { seq: prev_seq + 1, set: set.clone() };
    if let Ok(txt) = serde_json::to_string(&ctl) {
        let tmp = build_dir.join("control.json.tmp");
        if std::fs::write(&tmp, txt).is_ok() {
            return std::fs::rename(&tmp, control_path(build_dir)).is_ok();
        }
    }
    false
}

/// Persist the FULL resumable state (results + cohort map + cumulative clock), byte-compatible with
/// the Python tool — so nothing (cohorts, elapsed, per-family status) is lost across a restart/migration.
pub fn write_state_full(build_dir: &Path, st: &StateFile) {
    let _ = std::fs::create_dir_all(build_dir);
    let tmp = build_dir.join("state.json.tmp");
    if let Ok(txt) = serde_json::to_string(st) {
        if std::fs::write(&tmp, txt).is_ok() {
            let _ = std::fs::rename(&tmp, state_path(build_dir));
        }
    }
}

/// Load the full state document (cohort map + clock + results). Default if absent/unparseable.
pub fn read_state_full(build_dir: &Path) -> StateFile {
    let txt = match std::fs::read_to_string(state_path(build_dir)) {
        Ok(t) => t,
        Err(_) => return StateFile::default(),
    };
    serde_json::from_str(&txt).unwrap_or_default()
}

/// Convenience: just the per-family results (used by tests and simple callers).
#[allow(dead_code)]
pub fn read_state(build_dir: &Path) -> BTreeMap<String, Res> {
    read_state_full(build_dir).results
}

/// Append one durable failure record (JSON line). Append-only — never erased by a later success.
pub fn append_failure(build_dir: &Path, entry: &FailHist) {
    let _ = std::fs::create_dir_all(build_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fail_hist_path(build_dir))
    {
        if let Ok(line) = serde_json::to_string(entry) {
            let _ = writeln!(f, "{}", line);
        }
    }
}

/// Load the failure history (newest last). Tolerates partial/corrupt trailing lines.
pub fn read_failure_history(build_dir: &Path) -> Vec<FailHist> {
    let txt = match std::fs::read_to_string(fail_hist_path(build_dir)) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    txt.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<FailHist>(l).ok())
        .collect()
}

/// Append one line to events.jsonl (started/built/failed/venv) — the append-only stream external
/// web UIs tail. The value is a pre-built JSON object.
pub fn append_event(build_dir: &Path, ev: &serde_json::Value) {
    let _ = std::fs::create_dir_all(build_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(build_dir.join("events.jsonl"))
    {
        if let Ok(line) = serde_json::to_string(ev) {
            let _ = writeln!(f, "{}", line);
        }
    }
}

/// Atomically write a derived report file (migration.json / timings.json) next to status.json.
pub fn write_json_file(build_dir: &Path, name: &str, value: &serde_json::Value) {
    let _ = std::fs::create_dir_all(build_dir);
    let tmp = build_dir.join(format!("{}.tmp", name));
    if let Ok(txt) = serde_json::to_string_pretty(value) {
        if std::fs::write(&tmp, txt).is_ok() {
            let _ = std::fs::rename(&tmp, build_dir.join(name));
        }
    }
}

/// Write our PID so a later invocation can attach/stop us.
pub fn write_pid(build_dir: &Path) {
    let _ = std::fs::create_dir_all(build_dir);
    let _ = std::fs::write(pid_path(build_dir), format!("{}", std::process::id()));
}

pub fn clear_pid(build_dir: &Path) {
    let _ = std::fs::remove_file(pid_path(build_dir));
}

/// Read a running daemon's PID, but only if the process is actually alive (kill -0). A stale pidfile
/// (daemon crashed/rebooted) reads as None so we don't refuse to start over it.
pub fn read_daemon_pid(build_dir: &Path) -> Option<i32> {
    let txt = std::fs::read_to_string(pid_path(build_dir)).ok()?;
    let pid: i32 = txt.trim().parse().ok()?;
    if pid <= 0 {
        return None;
    }
    // signal 0 = liveness probe
    let alive = libc_kill(pid, 0) == 0;
    if alive {
        Some(pid)
    } else {
        None
    }
}

// Minimal libc bindings (avoid pulling the whole `libc` crate just for kill()).
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Res, StateFile};
    #[test]
    fn state_full_roundtrip_preserves_cohorts_and_clock() {
        let dir = std::env::temp_dir().join(format!("_stfull_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut st = StateFile { elapsed_so_far: 79249.0, build_dir: "/x".into(), ..Default::default() };
        st.results.insert("ofl/x".into(), Res { slug: "ofl/x".into(), status: "failed".into(),
            error: "venv: pip install rc=1".into(), ..Default::default() });
        st.cohort_members.insert("base".into(), vec!["ofl/x".into(), "ofl/y".into()]);
        st.cohort_members.insert("c-abc".into(), vec!["ofl/z".into()]);
        st.cohort_reqs.insert("c-abc".into(), "gftools\ncompreffor".into());
        write_state_full(&dir, &st);
        let back = read_state_full(&dir);
        assert_eq!(back.elapsed_so_far, 79249.0, "cumulative clock must survive");
        assert_eq!(back.cohort_members.get("base").unwrap().len(), 2, "cohort members survive");
        assert_eq!(back.cohort_reqs.get("c-abc").unwrap(), "gftools\ncompreffor", "cohort reqs survive");
        assert_eq!(back.results.get("ofl/x").unwrap().status, "failed", "per-family status survives");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
