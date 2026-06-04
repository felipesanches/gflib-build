//! Mirror cloning (R3 / `--mirror-missing`): clone a missing upstream repo into the archive as a
//! bare mirror — APPEND-ONLY (we only ever add, never delete/modify). Abortable (polled against the
//! stop flag so `--stop`/shutdown terminates an in-flight clone promptly) and auto-retrying on
//! TRANSIENT network errors only. Ported from the Python `git_clone_mirror` / `_clone_mirror_once`.

use crate::classify::is_transient_clone_error;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn abortable_wait(secs: u64, stop: &AtomicBool) -> bool {
    // returns true if aborted
    for _ in 0..(secs * 10) {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn clone_once(url: &str, dest: &Path, timeout: u64, stop: &AtomicBool) -> (bool, bool, String) {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut child = match Command::new("git")
        .args(["clone", "--mirror", "--quiet", url, &dest.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, false, format!("spawn git clone: {}", e)),
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut aborted = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if stop.load(Ordering::Relaxed) || Instant::now() > deadline {
                    aborted = stop.load(Ordering::Relaxed);
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            Err(_) => break,
        }
    }
    let out = child.wait_with_output().ok();
    let ok = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
    let mut err = out.map(|o| String::from_utf8_lossy(&o.stderr).to_string()).unwrap_or_default();
    if !ok {
        let _ = std::fs::remove_dir_all(dest); // never leave a partial mirror behind
        if aborted {
            err = "aborted".into();
        } else if err.trim().is_empty() {
            err = format!("timed out after {}s", timeout);
        }
    }
    (ok, aborted, err)
}

/// Clone `url` into the bare mirror `dest`, abortable + auto-retrying on transient errors.
pub fn clone_mirror(url: &str, dest: &Path, timeout: u64, stop: &AtomicBool, attempts: u32) -> Result<(), String> {
    let attempts = attempts.max(1);
    let mut last_err = String::new();
    for attempt in 1..=attempts {
        let (ok, aborted, err) = clone_once(url, dest, timeout, stop);
        if ok {
            return Ok(());
        }
        if aborted {
            return Err("aborted".into());
        }
        last_err = err.clone();
        if attempt >= attempts || !is_transient_clone_error(&err) {
            break;
        }
        if abortable_wait((2 * attempt as u64).min(10), stop) {
            return Err("aborted".into());
        }
    }
    if attempts > 1 && is_transient_clone_error(&last_err) {
        last_err = format!("{}  (after {} attempts)", last_err.trim_end(), attempts);
    }
    Err(last_err)
}
