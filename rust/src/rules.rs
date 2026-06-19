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
            let list: Vec<String> = cmds
                .iter()
                .filter_map(|c| c.as_str().map(|s| s.to_string()))
                // QA is handled by gflib-build's own --fontspector pass, consistently for ALL families,
                // so any fontspector/fontbakery install-or-invoke in an upstream rule is DISABLED here.
                .filter(|cmd| !is_qa_command(cmd))
                .collect();
            if !list.is_empty() {
                out.insert(slug.clone(), list);
            }
        }
    }
    out
}

/// True if a pre-build command installs or invokes fontbakery/fontspector (which we disable — QA is
/// our job via --fontspector, run identically for every family).
fn is_qa_command(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    c.contains("fontbakery") || c.contains("fontspector")
}

/// If a pre-build command would invoke Python — a Python interpreter, pip, a `*.py` script, or a known
/// Python-based font tool — return the offending token, so the Rust-only policy (python_policy=off /
/// unauthorized) can refuse it with a clear cause instead of silently running Python. The command string
/// is split into segments on the shell separators `; | &` (handling both `a && b` AND glued `a&&b`); in
/// each segment only the COMMAND token is inspected — the first token after skipping leading `VAR=val`
/// assignments and `env`/`sudo`/`nice`/`exec`/`time`/`command`/`xargs` wrappers — so a
/// `cp scripts/gen.py out/` argument is NOT flagged but `python3 gen.py` is.
///
/// Conservative by design (we err toward refusing), but it is a STATIC heuristic, not a shell parser:
/// Python hidden inside a subshell `$(…)`, backticks, a `bash -c '…'` / `sh -c '…'` string, a variable
/// (`$PY`), or a called shell script (`sh build.sh`) is NOT detected. Callers log the decision so the
/// analysis round can audit it; the eventual fix for those cases is to port the pre-build to shell/Rust.
pub fn rule_needs_python(cmd: &str) -> Option<String> {
    const PY_TOOLS: &[&str] = &[
        "fontmake", "fonttools", "ttx", "ufo2ft", "cu2qu",
        "psautohint", "glyphs2ufo", "statmake", "afdko", "makeotf", "buildmasterotfs",
    ];
    let looks_python = |tok: &str| -> bool {
        let base = tok.rsplit('/').next().unwrap_or(tok).to_lowercase();
        base == "python" || base == "python2" || base.starts_with("python3")
            || matches!(base.as_str(), "pip" | "pip2" | "pip3" | "pipx")
            || base == "gftools" || base.starts_with("gftools-")
            || base.ends_with(".py")
            || PY_TOOLS.contains(&base.as_str())
    };
    // a run of ; | & delimits command segments; the command is the first real token of each segment
    for seg in cmd.split(|c| c == ';' || c == '|' || c == '&') {
        for tok in seg.split_whitespace() {
            // skip a leading `VAR=val` env assignment or a wrapper command — the real command is ahead
            let is_assign = tok.contains('=') && !tok.starts_with('-') && !tok.contains('/');
            let is_wrapper = matches!(tok, "env" | "sudo" | "nice" | "exec" | "time" | "command" | "xargs" | "then" | "do" | "!");
            if is_assign || is_wrapper {
                continue;
            }
            // the first real command token of this segment decides it
            if looks_python(tok) {
                return Some(tok.to_string());
            }
            break;
        }
    }
    None
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
    // venv bin = python's parent WITHOUT resolving symlinks (canonicalize would follow the
    // venv/bin/python symlink to the system /usr/bin and miss the pinned fontmake/gftools/python)
    let bindir = {
        let p = Path::new(python);
        if p.is_absolute() { p.parent().map(|d| d.to_path_buf()) } else { None }
    };
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
    fn rule_needs_python_flags_python_commands_only() {
        // genuine Python invocations (command position) → flagged with the offending token
        assert_eq!(rule_needs_python("python3 scripts/build.py").as_deref(), Some("python3"));
        assert_eq!(rule_needs_python("./gen.py --out x").as_deref(), Some("./gen.py"));
        assert_eq!(rule_needs_python("pip install foo").as_deref(), Some("pip"));
        assert_eq!(rule_needs_python("gftools-add-ds-subsets sources/x.glyphs").as_deref(), Some("gftools-add-ds-subsets"));
        assert_eq!(rule_needs_python("fontmake -g x.glyphs").as_deref(), Some("fontmake"));
        // a Python command later in a chain (after a separator) is still caught
        assert_eq!(rule_needs_python("mkdir build && python3 gen.py").as_deref(), Some("python3"));
        // …even when the separator is GLUED to adjacent tokens (no surrounding whitespace)
        assert_eq!(rule_needs_python("mkdir x&&python3 gen.py").as_deref(), Some("python3"));
        assert_eq!(rule_needs_python("cat a|python3 -").as_deref(), Some("python3"));
        // a VAR=val prefix / wrapper doesn't hide the real command
        assert_eq!(rule_needs_python("FOO=1 python3 gen.py").as_deref(), Some("python3"));
        assert_eq!(rule_needs_python("env python3 gen.py").as_deref(), Some("python3"));
        // pure-shell rules are allowed (no false positives)
        assert_eq!(rule_needs_python("cp scripts/gen.py sources/"), None); // .py is an ARGUMENT, not the command
        assert_eq!(rule_needs_python("set -o pipefail && cp a b"), None);  // "pipefail" must not match "pip"
        assert_eq!(rule_needs_python("mkdir -p out && sed -i s/a/b/ x.fea"), None);
        assert_eq!(rule_needs_python("make sources"), None);              // shell-ish; not flagged (logged for audit)
    }
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

#[cfg(test)]
mod qa_tests {
    use super::is_qa_command;
    #[test]
    fn qa_commands_filtered() {
        assert!(is_qa_command("python3 -m pip install fontbakery"));
        assert!(is_qa_command("fontspector --profile googlefonts x.ttf"));
        assert!(is_qa_command("FontBakery check-googlefonts *.ttf"));
        assert!(!is_qa_command("python3 build.py"));
        assert!(!is_qa_command("pip install ufo2ft"));
    }
}
