//! M0 — record WHICH compiler and WHICH build orchestrator produced (or attempted) every family,
//! with exact versions, so the Python→Rust migration is measurable. Two independent axes:
//!   * compiler:    fontmake (Python) | fontc (Rust)
//!   * orchestrator: builder2 (`gftools.builder`, Python) | builder3 (Rust-native)
//! A dev `fontc`/`builder3` built from a git checkout also records its source commit, so a result
//! can be pinned to the exact binary it came from. Ported 1:1 from the Python `compiler_version_str`
//! / `builder_version_str`; never panics — falls back to the backend/builder name.

use std::path::Path;
use std::process::Command;

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// If `bin` lives inside a git checkout (…/<repo>/target/release/<bin>), append "(git <short>)".
fn append_source_commit(bin: &str, ver: &mut String) {
    let mut src = match Path::new(bin).canonicalize() {
        Ok(p) => p,
        Err(_) => return,
    };
    for _ in 0..4 {
        if !src.pop() {
            return;
        }
        if src.join(".git").exists() {
            if let Ok(o) = Command::new("git")
                .args(["-C", &src.to_string_lossy(), "rev-parse", "--short", "HEAD"])
                .output()
            {
                let sh = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if o.status.success() && !sh.is_empty() && !ver.contains(&sh) {
                    ver.push_str(&format!(" (git {})", sh));
                }
            }
            return;
        }
    }
}

/// Exact compiler version for the given backend. `python` runs fontmake's package-version query in
/// the build interpreter; `fontc_bin` is queried with `--version`.
pub fn compiler_version_str(backend: &str, python: &str, fontc_bin: Option<&str>) -> String {
    match backend {
        "fontc" => {
            if let Some(bin) = fontc_bin {
                if let Ok(o) = Command::new(bin).arg("--version").output() {
                    let txt = format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                    let mut ver = if txt.trim().is_empty() {
                        "fontc (unknown version)".to_string()
                    } else {
                        first_line(&txt)
                    };
                    append_source_commit(bin, &mut ver);
                    return ver;
                }
            }
            "fontc".to_string()
        }
        "fontmake" => {
            if let Ok(o) = Command::new(python)
                .args([
                    "-c",
                    "import importlib.metadata as m; print('fontmake '+m.version('fontmake'))",
                ])
                .output()
            {
                let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !v.is_empty() {
                    return v;
                }
            }
            "fontmake (unknown version)".to_string()
        }
        "both" => format!(
            "{}  +  {}",
            compiler_version_str("fontc", python, fontc_bin),
            compiler_version_str("fontmake", python, fontc_bin)
        ),
        other => other.to_string(),
    }
}

/// Exact orchestrator version. builder2 -> the `gftools` package version in the build interpreter;
/// builder3 -> the Rust binary's `--version` (+ source commit when built from a checkout).
pub fn builder_version_str(builder: &str, python: &str, builder3_bin: Option<&str>) -> String {
    if builder == "builder3" {
        if let Some(bin) = builder3_bin {
            if let Ok(o) = Command::new(bin).arg("--version").output() {
                let txt = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                let mut ver = if txt.trim().is_empty() {
                    "builder3 (unknown version)".to_string()
                } else {
                    first_line(&txt)
                };
                append_source_commit(bin, &mut ver);
                return if ver.to_lowercase().contains("builder3") {
                    ver
                } else {
                    format!("gftools-builder3 {}", ver)
                };
            }
        }
        return "builder3".to_string();
    }
    // builder2: gftools package version in the build interpreter that runs gftools.builder
    if let Ok(o) = Command::new(python)
        .args([
            "-c",
            "import importlib.metadata as m; print('gftools-builder2 '+m.version('gftools'))",
        ])
        .output()
    {
        let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    "gftools-builder2 (unknown version)".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_backend_falls_back_to_name() {
        // an unrecognised backend returns its own name (never panics)
        assert_eq!(compiler_version_str("nonesuch", "python3", None), "nonesuch");
    }
    #[test]
    fn fontc_without_binary_is_graceful() {
        // no fontc_bin -> the literal "fontc", not a crash
        assert_eq!(compiler_version_str("fontc", "python3", None), "fontc");
    }
    #[test]
    fn builder3_without_binary_is_graceful() {
        assert_eq!(builder_version_str("builder3", "python3", None), "builder3");
    }
}
