//! diffenator3-based semantic comparison of a freshly-built font against the font shipped in the
//! google/fonts working copy. Pure Rust (diffenator3-lib + ttj) — no Python. This is a real
//! correctness signal, beyond the sha256 `compare` feature: it reports table/kerning/cmap changes and
//! render-level glyph/word differences, the same engine fontc_crater-style diffing uses.
//!
//! The family-level results are produced by an async, niced worker (see build.rs), mirroring the
//! fontspector QA feature, and aggregated into a [`DiffView`] for the UI.

use crate::model::{DiffFamily, DiffView};
use diffenator3_lib::dfont::DFont;
use diffenator3_lib::render::encodedglyphs::modified_encoded_glyphs;
use diffenator3_lib::render::test_font_words;
use diffenator3_lib::structs::CmapDiff;
use std::path::Path;

/// Structured diff of two TTFs: `before` = shipped, `after` = freshly built.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FontDiff {
    pub identical: bool,
    pub table_changes: usize,   // changed top-level table groups (head/OS2/name/GSUB/GPOS/STAT/…)
    pub kern_changes: usize,    // changed kerning entries
    pub cmap_missing: usize,    // codepoints encoded in the shipped font but NOT the built one
    pub cmap_new: usize,        // codepoints newly encoded in the built font
    pub modified_glyphs: usize, // shared encoded glyphs that render differently
    pub modified_words: usize,  // shaped words that render differently
    pub worst_pixels: usize,    // max differing pixels across glyph/word renders (severity proxy)
}

impl FontDiff {
    /// One-line human summary (UI cell / log).
    pub fn summary(&self) -> String {
        if self.identical {
            return "identical".into();
        }
        let mut parts = Vec::new();
        if self.table_changes > 0 {
            parts.push(format!("{} tables", self.table_changes));
        }
        if self.kern_changes > 0 {
            parts.push(format!("{} kern", self.kern_changes));
        }
        if self.cmap_missing > 0 || self.cmap_new > 0 {
            parts.push(format!("cmap -{}/+{}", self.cmap_missing, self.cmap_new));
        }
        if self.modified_glyphs > 0 {
            parts.push(format!("{} glyphs", self.modified_glyphs));
        }
        if self.modified_words > 0 {
            parts.push(format!("{} words", self.modified_words));
        }
        format!("differs: {}", parts.join(", "))
    }
}

/// Diff two TTF files. diffenator3 calls `FontRef::new().expect(..)` and rasterizes glyphs, so a
/// malformed/edge-case font can panic — the whole diff runs under `catch_unwind` and a panic is
/// reported as an error rather than taking down the worker thread.
pub fn diff_ttf(before: &Path, after: &Path) -> Result<FontDiff, String> {
    let before = before.to_path_buf();
    let after = after.to_path_buf();
    let result = std::panic::catch_unwind(move || -> Result<FontDiff, String> {
        let bytes_a = std::fs::read(&before).map_err(|e| format!("read shipped: {e}"))?;
        let bytes_b = std::fs::read(&after).map_err(|e| format!("read built: {e}"))?;
        let font_a = DFont::new(&bytes_a);
        let font_b = DFont::new(&bytes_b);

        // location-independent table + kerning diffs (serde_json::Value; empty object = no change).
        // no_match=false lets ttj align glyph names between the two fonts (right for before/after).
        const MAX: usize = 5000;
        let ra = font_a.fontref();
        let rb = font_b.fontref();
        let tables = ttj::table_diff(&ra, &rb, MAX, false);
        let kerns = ttj::kern_diff(&ra, &rb, MAX, false);
        let cmap = CmapDiff::new(&font_a, &font_b);

        // render-level diffs (default instance; rasterized) — fuzzy by nature, scored by differing pixels.
        let glyphs = modified_encoded_glyphs(&font_a, &font_b);
        let words = test_font_words(&font_a, &font_b, &[]); // &[] = built-in per-script word lists

        let table_changes = tables.as_object().map(|o| o.len()).unwrap_or(0);
        let kern_changes = kerns.as_object().map(|o| o.len()).unwrap_or(0);
        let modified_words: usize = words.values().map(|v| v.len()).sum();
        let worst_pixels = glyphs
            .iter()
            .map(|g| g.differing_pixels)
            .chain(words.values().flatten().map(|d| d.differing_pixels))
            .max()
            .unwrap_or(0);
        let identical = table_changes == 0
            && kern_changes == 0
            && !cmap.is_some()
            && glyphs.is_empty()
            && modified_words == 0;

        Ok(FontDiff {
            identical,
            table_changes,
            kern_changes,
            cmap_missing: cmap.missing.len(),
            cmap_new: cmap.new.len(),
            modified_glyphs: glyphs.len(),
            modified_words,
            worst_pixels,
        })
    });
    match result {
        Ok(inner) => inner,
        Err(_) => Err("diffenator3 panicked while diffing".to_string()),
    }
}

/// Per-font outcome inside a family's diff result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OneFont {
    pub file: String,
    pub status: String, // "identical" | "differs: …" | "no shipped font" | "error: …"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<FontDiff>,
}

/// Diff every built font of a family against its shipped counterpart in the google/fonts clone.
/// `built` maps shipped basename -> freshly-built path (the same map `compare` uses). Returns the
/// per-font outcomes and the family-level worst status ("differs" if any font differs, else
/// "identical", or "error"/"no shipped font" when nothing could be compared).
pub fn diff_family(google_fonts: &Path, slug: &str, built: &std::collections::BTreeMap<String, std::path::PathBuf>) -> (String, Vec<OneFont>) {
    let fam_dir = google_fonts.join(slug);
    let mut fonts = Vec::new();
    let (mut any_diff, mut any_ok) = (false, false);
    for (basename, built_path) in built {
        let shipped = fam_dir.join(basename);
        if !shipped.is_file() {
            fonts.push(OneFont { file: basename.clone(), status: "no shipped font".into(), diff: None });
            continue;
        }
        match diff_ttf(&shipped, built_path) {
            Ok(d) => {
                any_ok = true;
                if !d.identical {
                    any_diff = true;
                }
                let status = d.summary();
                fonts.push(OneFont { file: basename.clone(), status, diff: Some(d) });
            }
            Err(e) => fonts.push(OneFont { file: basename.clone(), status: format!("error: {e}"), diff: None }),
        }
    }
    let family_status = if any_diff {
        "differs"
    } else if any_ok {
        "identical"
    } else if fonts.iter().all(|f| f.status == "no shipped font") && !fonts.is_empty() {
        "no shipped font"
    } else {
        "error"
    }
    .to_string();
    (family_status, fonts)
}

/// Aggregate per-family JSON results (written by the worker to `<build_dir>/diffenator3/<slug__>.json`)
/// into the [`DiffView`] the snapshot/UI consume.
pub fn aggregate(diff_dir: &Path) -> DiffView {
    let mut families: Vec<DiffFamily> = Vec::new();
    let (mut identical, mut differs, mut errored) = (0usize, 0usize, 0usize);
    if let Ok(rd) = std::fs::read_dir(diff_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if p.file_name().and_then(|n| n.to_str()) == Some("_summary.json") {
                continue;
            }
            let Ok(txt) = std::fs::read_to_string(&p) else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
            let slug = v.get("slug").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("error").to_string();
            let summary = v.get("summary").and_then(|s| s.as_str()).unwrap_or("").to_string();
            match status.as_str() {
                "identical" => identical += 1,
                "differs" => differs += 1,
                _ => errored += 1,
            }
            families.push(DiffFamily { slug, status, summary });
        }
    }
    // differs first (most interesting), then errors, then identical; stable by slug within a status
    families.sort_by(|a, b| {
        let rank = |s: &str| match s {
            "differs" => 0,
            "identical" => 2,
            _ => 1,
        };
        rank(&a.status).cmp(&rank(&b.status)).then(a.slug.cmp(&b.slug))
    });
    DiffView { families_checked: families.len(), identical, differs, errored, families }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A font diffed against itself must be reported identical (validates the whole diff pipeline:
    /// tables, kerning, cmap, glyph + word renders). Skips cleanly if no built font is present.
    #[test]
    fn self_diff_is_identical() {
        // first .ttf under the local build output, if any (relative to this crate dir)
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gflib-data/build/out");
        let mut found: Option<std::path::PathBuf> = None;
        if let Ok(rd) = std::fs::read_dir(&out) {
            for fam in rd.flatten() {
                if let Ok(files) = std::fs::read_dir(fam.path()) {
                    for f in files.flatten() {
                        if f.path().extension().and_then(|e| e.to_str()) == Some("ttf") {
                            found = Some(f.path());
                            break;
                        }
                    }
                }
                if found.is_some() {
                    break;
                }
            }
        }
        let Some(ttf) = found else {
            eprintln!("self_diff_is_identical: no built font present — skipping");
            return;
        };
        let d = diff_ttf(&ttf, &ttf).expect("self-diff should not error");
        assert!(d.identical, "a font diffed against itself must be identical: {d:?}");
        assert_eq!(d.summary(), "identical");
    }
}
