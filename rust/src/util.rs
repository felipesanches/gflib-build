//! Small shared helpers: human-readable sizes/durations, slug<->path mapping, wall-clock, and a
//! recursive directory byte count (used for the header's disk figure, mirroring the Python `du`).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bytes -> "1.5GiB" etc. Matches the Python `human()` so headers read identically across ports.
pub fn human(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", n, units[0])
    } else {
        format!("{:.1}{}", v, units[i])
    }
}

/// Seconds -> "HH:MM:SS" (cumulative elapsed clock).
pub fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Current wall-clock as fractional seconds since the epoch (matches Python's time.time()).
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// "ofl/roboto" -> "ofl__roboto" for a flat log filename (same convention as the Python tool).
pub fn slug_to_logname(slug: &str) -> String {
    slug.replace('/', "__")
}

/// Total bytes a directory tree occupies. Best-effort: unreadable entries are skipped. Used only on
/// a background thread (never the render path), mirroring the Python `_measure_dir`.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let rd = match std::fs::read_dir(&p) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(ent.path());
            } else if let Ok(md) = ent.metadata() {
                total += md.len();
            }
        }
    }
    total
}

/// Free bytes on the filesystem holding `path` (via `statvfs`). Returns 0 on failure.
pub fn free_bytes(path: &Path) -> u64 {
    // Shell out to `df` rather than bind libc statvfs — robust and dependency-free.
    let out = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output();
    if let Ok(o) = out {
        let txt = String::from_utf8_lossy(&o.stdout);
        if let Some(line) = txt.lines().nth(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // df -k columns: Filesystem 1K-blocks Used Available ...  (Available is index 3)
            if cols.len() >= 4 {
                if let Ok(kb) = cols[3].parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(512), "512B");
        assert_eq!(human(1024), "1.0KiB");
        assert_eq!(human(3 << 30), "3.0GiB");
    }
    #[test]
    fn hms_fmt() {
        assert_eq!(hms(0.0), "00:00:00");
        assert_eq!(hms(3661.0), "01:01:01");
        assert_eq!(hms(-5.0), "00:00:00");
    }
    #[test]
    fn slug_log() {
        assert_eq!(slug_to_logname("ofl/roboto"), "ofl__roboto");
    }
}
