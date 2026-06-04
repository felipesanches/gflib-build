//! Per-family pre-build commands (`build_rules.json`) — R3. Some upstream sources must be generated
//! or pre-compiled (a glyphs file produced by a script, a filename-case fixup, …) BEFORE the
//! gftools-builder runs. Faithful port of the Python `load_build_rules` / `run_pre_build`: shell
//! commands, `cwd` = the extracted source, the build venv's `bin` first on `PATH` so the pinned
//! fontmake/fonttools/gftools/python are used.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Load `build_rules.json` → {slug: [pre_build commands]}. Empty if absent/unparseable.
pub fn load_build_rules(path: &Path) -> HashMap<String, Vec<String>> {
    let txt = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let rules = match v.get("rules").and_then(|r| r.as_object()) {
        Some(r) => r,
        None => return HashMap::new(),
    };
    let mut out = HashMap::new();
    for (slug, spec) in rules {
        if let Some(cmds) = spec.get("pre_build").and_then(|c| c.as_array()) {
            let list: Vec<String> =
                cmds.iter().filter_map(|c| c.as_str().map(|s| s.to_string())).collect();
            if !list.is_empty() {
                out.insert(slug.clone(), list);
            }
        }
    }
    out
}

/// Run a family's pre-build commands. Ok(()) on success; Err(msg) on the first failing command. A
/// non-zero exit fails the family with a clear `pre-build` error (so it isn't silently mis-built).
pub fn run_pre_build(
    work: &Path,
    python: &str,
    cmds: &[String],
    log_path: &Path,
    timeout: Option<u64>,
) -> Result<(), String> {
    if cmds.is_empty() {
        return Ok(());
    }
    let bindir = Path::new(python).canonicalize().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let path_env = match (bindir, std::env::var("PATH").ok()) {
        (Some(b), Some(p)) => format!("{}:{}", b.display(), p),
        (Some(b), None) => b.display().to_string(),
        (None, Some(p)) => p,
        (None, None) => String::new(),
    };
    let to = Duration::from_secs(timeout.unwrap_or(3600));
    for cmd in cmds {
        log_line(log_path, &format!("\n===== pre-build: {} =====\n# cwd={}", cmd, work.display()));
        let logf = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|e| format!("pre-build open log: {}", e))?;
        let logf2 = logf.try_clone().ok();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(work)
            .env("SOURCE_DATE_EPOCH", "0")
            .env("PATH", &path_env)
            .stdout(Stdio::from(logf))
            .stderr(logf2.map(Stdio::from).unwrap_or(Stdio::null()))
            .spawn()
            .map_err(|e| format!("pre-build could not run: {} ({})", cmd, e))?;
        let deadline = Instant::now() + to;
        loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    if !st.success() {
                        return Err(format!("pre-build failed (rc={:?}): {}", st.code(), cmd));
                    }
                    break;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        return Err(format!("pre-build timed out: {}", cmd));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(format!("pre-build wait: {} ({})", cmd, e)),
            }
        }
    }
    Ok(())
}

fn log_line(log_path: &Path, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "{}", msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loads_rules_and_runs_a_pre_build() {
        let dir = std::env::temp_dir().join(format!("_rules_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let rf = dir.join("build_rules.json");
        std::fs::write(&rf, r#"{"rules":{"ofl/x":{"pre_build":["touch generated.txt"],"note":"gen"},
                                        "ofl/empty":{"note":"no cmds"}}}"#).unwrap();
        let rules = load_build_rules(&rf);
        assert_eq!(rules.get("ofl/x").unwrap(), &vec!["touch generated.txt".to_string()]);
        assert!(!rules.contains_key("ofl/empty")); // no pre_build -> not included

        // run it: the command creates generated.txt in the work dir
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let log = dir.join("x.log");
        let r = run_pre_build(&work, "/usr/bin/python3", rules.get("ofl/x").unwrap(), &log, Some(60));
        assert!(r.is_ok(), "pre-build should succeed: {:?}", r);
        assert!(work.join("generated.txt").is_file(), "the pre-build command must have run in cwd=work");

        // a failing command is reported
        let bad = run_pre_build(&work, "/usr/bin/python3", &["exit 3".to_string()], &log, Some(60));
        assert!(bad.is_err() && bad.unwrap_err().contains("pre-build failed"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
