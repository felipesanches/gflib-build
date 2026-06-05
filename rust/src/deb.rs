//! `--export-deb`: draft a Debian source-package tree (`debian/`) for every successfully-built
//! family, so gflib-build's results can feed the self-hosted complementary apt repo. Read-only
//! w.r.t. the archive and the build outputs; only writes under
//! `<build_dir>/packaging/<slug__>/debian/`. See `docs/debian-packaging-plan.md`.
//!
//! A family is drafted only when it BOTH (a) has `status=="built"` in state.json AND (b) still has
//! >=1 `.ttf`/`.otf` on disk under `<build_dir>/out/<logname>/` — out dirs are deleted after a build
//! that ran without `--keep-fonts`, so "built" alone does NOT imply fonts are present.

use crate::{config, discover, model, persist, util};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAINTAINER: &str = "Google Fonts Deb-Packaging (gflib-build) <juca@members.fsf.org>";

pub fn run_export_deb(cfg: &config::Config) {
    let mut cfg = cfg.clone();
    if !cfg.archive.is_dir() {
        if let Some(a) = discover::detect_archive(&cfg.data_dir) {
            cfg.archive = PathBuf::from(a);
        }
    }
    let st = persist::read_state_full(&cfg.build_dir);
    let (fams, _total, _skipped) = match cfg.source.as_str() {
        "archive" => discover::discover_archive(&cfg.archive, &cfg.archive_rev, cfg.jobs, None),
        _ => match &cfg.google_fonts {
            Some(gf) => discover::discover_metadata(gf),
            None => {
                eprintln!("--export-deb with --source metadata needs --google-fonts");
                return;
            }
        },
    };
    let by_slug: BTreeMap<&str, &model::Family> =
        fams.iter().map(|f| (f.slug.as_str(), f)).collect();

    let pkg_root = cfg.build_dir.join("packaging");
    let gf = cfg.google_fonts.clone();
    let mut index: Vec<serde_json::Value> = Vec::new();
    let (mut exported, mut no_fam, mut no_fonts, mut lic_fallback) = (0usize, 0usize, 0usize, 0usize);

    for (slug, res) in &st.results {
        if res.status != "built" {
            continue;
        }
        let fam = match by_slug.get(slug.as_str()) {
            Some(f) => *f,
            None => {
                no_fam += 1;
                continue;
            }
        };

        // gate on on-disk font presence (out dir may have been pruned post-build)
        let out_dir = cfg.build_dir.join("out").join(util::slug_to_logname(slug));
        let fonts = collect_fonts(&out_dir);
        if fonts.is_empty() {
            no_fonts += 1;
            continue;
        }
        let font_names: Vec<String> = fonts
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();
        let has_ttf = font_names.iter().any(|n| n.to_ascii_lowercase().ends_with(".ttf"));
        let has_otf = font_names.iter().any(|n| n.to_ascii_lowercase().ends_with(".otf"));

        let (spdx, dep5, fallback) = license_for(slug);
        if fallback {
            lic_fallback += 1;
        }
        let pkg = pkg_name(slug);
        let short = short_commit(&fam.commit);
        let epoch = if res.ended > 0.0 { res.ended } else { util::now() };
        let version = format!("0~gf{}.g{}-1", ymd(epoch), short);

        let debian = pkg_root.join(util::slug_to_logname(slug)).join("debian");
        if std::fs::create_dir_all(debian.join("source")).is_err() {
            eprintln!("skip {}: cannot create {}", slug, debian.display());
            continue;
        }

        write(&debian.join("control"), &control(&pkg, fam, spdx));
        let r = debian.join("rules");
        write(&r, &rules(&pkg, has_ttf, has_otf));
        set_exec(&r);
        write(
            &debian.join("copyright"),
            &copyright(fam, dep5, &copyright_holder(gf.as_deref(), slug)),
        );
        write(
            &debian.join("changelog"),
            &changelog(&pkg, &version, slug, &fam.commit, epoch),
        );
        write(&debian.join("watch"), &watch(&fam.url));
        write(&debian.join("source/format"), "3.0 (quilt)\n");
        write(
            &debian.join("gflib-provenance"),
            &provenance(slug, &pkg, spdx, fam, res, st.cohort_reqs.get(&res.cohort), &font_names),
        );

        index.push(serde_json::json!({
            "slug": slug, "package": pkg, "version": version, "license": spdx,
            "fonts": font_names.len(), "backend": res.backend,
            "compiler_version": res.compiler_version,
        }));
        exported += 1;
    }

    let _ = std::fs::create_dir_all(&pkg_root);
    if let Ok(txt) = serde_json::to_string_pretty(&index) {
        let _ = std::fs::write(pkg_root.join("index.json"), txt);
    }
    eprintln!(
        "export-deb: drafted {} package(s) under {}",
        exported,
        pkg_root.display()
    );
    eprintln!(
        "  skipped: {} built-but-not-in-worklist, {} built-but-no-fonts-on-disk (pruned without --keep-fonts){}",
        no_fam,
        no_fonts,
        if lic_fallback > 0 {
            format!(", {} license-fallback (prefix not ofl/ufl/apache -> OFL-1.1, review)", lic_fallback)
        } else {
            String::new()
        }
    );
}

// ---- helpers ----

/// Recursively collect .ttf/.otf files under `dir` (out/<logname>/ may nest fontc/ + fontmake/).
fn collect_fonts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let rd = match std::fs::read_dir(&p) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let e = ext.to_ascii_lowercase();
                if e == "ttf" || e == "otf" {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

/// License from the slug's license-dir prefix. Returns (spdx, dep5_id, used_fallback).
fn license_for(slug: &str) -> (&'static str, &'static str, bool) {
    match slug.split('/').next().unwrap_or("") {
        "ofl" => ("OFL-1.1", "OFL-1.1", false),
        "ufl" => ("UFL-1.0", "UFL-1.0", false),
        "apache" => ("Apache-2.0", "Apache-2.0", false),
        _ => ("OFL-1.1", "OFL-1.1", true), // dominant GF license; flagged for review
    }
}

/// Debian binary package name `fonts-gf-<family>` (lowercase, [a-z0-9.+-]; must start alnum).
fn pkg_name(slug: &str) -> String {
    let fam = slug.splitn(2, '/').nth(1).unwrap_or(slug);
    let mut s: String = fam
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while s.chars().next().map(|c| !c.is_ascii_alphanumeric()).unwrap_or(false) {
        s.remove(0);
    }
    format!("fonts-gf-{}", s)
}

fn short_commit(c: &str) -> String {
    let n = c.len().min(7);
    if n == 0 {
        "0000000".to_string()
    } else {
        c[..n].to_string()
    }
}

fn write(p: &Path, s: &str) {
    let _ = std::fs::write(p, s);
}

fn set_exec(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(p) {
        let mut perm = md.permissions();
        perm.set_mode(0o755);
        let _ = std::fs::set_permissions(p, perm);
    }
}

fn copyright_holder(google_fonts: Option<&Path>, slug: &str) -> String {
    if let Some(gf) = google_fonts {
        // OFL.txt/UFL.txt carry the real holder on their first "Copyright" line; the Apache LICENSE
        // is the license TEXT (its appendix has a "Copyright [yyyy] [name of copyright owner]"
        // placeholder), so skip placeholder lines and try a NOTICE file for Apache families.
        for lf in ["OFL.txt", "UFL.txt", "NOTICE", "LICENSE.txt", "LICENSE"] {
            if let Ok(txt) = std::fs::read_to_string(gf.join(slug).join(lf)) {
                for line in txt.lines() {
                    let t = line.trim();
                    if t.starts_with("Copyright") && !is_license_placeholder(t) {
                        return t.to_string();
                    }
                }
            }
        }
    }
    "Upstream authors".to_string()
}

/// True for boilerplate "Copyright" lines that are license-template placeholders, not a real holder.
fn is_license_placeholder(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("[yyyy]") || l.contains("[name of copyright owner]") || l.contains("[year]") || l.contains("<year>")
}

fn control(pkg: &str, fam: &model::Family, spdx: &str) -> String {
    // Built line-by-line (NOT via `\n\` continuation, which strips the leading whitespace that
    // control-file field folding and the long Description require).
    let name = if fam.name.is_empty() { pkg } else { fam.name.as_str() };
    let mut v: Vec<String> = vec![
        format!("Source: {}", pkg),
        "Section: fonts".into(),
        "Priority: optional".into(),
        format!("Maintainer: {}", MAINTAINER),
        "Build-Depends: debhelper-compat (= 13),".into(),
        " fontmake,".into(),
        " gftools,".into(),
        " python3".into(),
        "Standards-Version: 4.6.2".into(),
        "Rules-Requires-Root: no".into(),
    ];
    if !fam.url.is_empty() {
        v.push(format!("Homepage: {}", fam.url));
        v.push(format!("Vcs-Browser: {}", fam.url));
    }
    v.push(String::new()); // blank line between source and binary paragraph
    v.push(format!("Package: {}", pkg));
    v.push("Architecture: all".into());
    v.push("Multi-Arch: foreign".into());
    v.push("Depends: ${misc:Depends}".into());
    v.push(format!("Description: {} -- Google Fonts, reproducible build", name));
    v.push(format!(" {}, packaged from upstream source ({}) and built from the pinned", name, spdx));
    v.push(" commit with the recorded gflib-build recipe (see debian/gflib-provenance).".into());
    v.push(" .".into());
    v.push(" Part of the self-hosted Google Fonts deb-packaging collection; the build is".into());
    v.push(" reproducible from the recorded manifest.".into());
    v.join("\n") + "\n"
}

fn rules(pkg: &str, has_ttf: bool, has_otf: bool) -> String {
    let fam = pkg.strip_prefix("fonts-gf-").unwrap_or(pkg);
    let subdir = format!("gf-{}", fam);
    let mut v: Vec<String> = vec![
        "#!/usr/bin/make -f".into(),
        "# Built from upstream source via the recorded gflib-build recipe.".into(),
        "# Source is fetched from the local repo archive mirror at the pinned commit".into(),
        "# (see debian/gflib-provenance); the metadata references the real upstream.".into(),
        String::new(),
        "%:".into(),
        "\tdh $@".into(),
        String::new(),
        "override_dh_auto_build:".into(),
        "\t# DRAFT: run pre_build (build_rules) then gftools-builder / fontc with the".into(),
        "\t# family config at the pinned commit. Wired in a later stage (sbuild + wheelhouse).".into(),
        "\ttrue".into(),
        String::new(),
        "override_dh_auto_install:".into(),
    ];
    if has_ttf {
        v.push(format!("\tinstall -d debian/{}/usr/share/fonts/truetype/{}", pkg, subdir));
        v.push(format!(
            "\tif ls built-fonts/*.ttf >/dev/null 2>&1; then install -m644 built-fonts/*.ttf debian/{}/usr/share/fonts/truetype/{}; fi",
            pkg, subdir
        ));
    }
    if has_otf {
        v.push(format!("\tinstall -d debian/{}/usr/share/fonts/opentype/{}", pkg, subdir));
        v.push(format!(
            "\tif ls built-fonts/*.otf >/dev/null 2>&1; then install -m644 built-fonts/*.otf debian/{}/usr/share/fonts/opentype/{}; fi",
            pkg, subdir
        ));
    }
    if !has_ttf && !has_otf {
        v.push("\ttrue".into());
    }
    v.join("\n") + "\n"
}

fn copyright(fam: &model::Family, dep5: &str, holder: &str) -> String {
    let stanza = match dep5 {
        "Apache-2.0" => "License: Apache-2.0\n On Debian systems, the full text of the Apache 2.0 license can be found in\n /usr/share/common-licenses/Apache-2.0.\n",
        "UFL-1.0" => "License: UFL-1.0\n Licensed under the Ubuntu Font Licence 1.0. The full text is distributed with\n the upstream source (see Source) as UFL.txt.\n",
        _ => "License: OFL-1.1\n This Font Software is licensed under the SIL Open Font License, Version 1.1.\n Available with a FAQ at https://openfontlicense.org ; the full text is\n distributed with the upstream source (see Source) as OFL.txt.\n",
    };
    format!(
        "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n\
Upstream-Name: {name}\n\
Source: {url}\n\
\n\
Files: *\n\
Copyright: {holder}\n\
License: {dep5}\n\
\n\
Files: debian/*\n\
Copyright: {holder}\n\
License: {dep5}\n\
\n\
{stanza}",
        name = fam.name,
        url = fam.url,
        holder = holder,
        dep5 = dep5,
        stanza = stanza,
    )
}

fn changelog(pkg: &str, version: &str, slug: &str, commit: &str, epoch: f64) -> String {
    // Per-line (NOT `\n\` continuation): dpkg requires "  * " change lines and a " -- " trailer,
    // and the continuation stripping would drop exactly that required leading whitespace.
    let v: Vec<String> = vec![
        format!("{} ({}) unstable; urgency=low", pkg, version),
        String::new(),
        "  * Draft package generated by gflib-build --export-deb from a reproducible".into(),
        format!("    build of {} at upstream commit {}.", slug, commit),
        String::new(),
        format!(" -- {}  {}", MAINTAINER, rfc2822(epoch)),
    ];
    v.join("\n") + "\n"
}

fn watch(url: &str) -> String {
    if url.is_empty() {
        return "version=4\n# no upstream URL recorded\n".to_string();
    }
    format!(
        "version=4\n\
# Track upstream tags. The build sources from the local archive mirror (see\n\
# debian/gflib-provenance); this references the real upstream for provenance.\n\
opts=\"mode=git, pgpmode=none\" \\\n\
  {url} refs/tags/v?([\\d.]+)\n",
        url = url,
    )
}

fn provenance(
    slug: &str,
    pkg: &str,
    spdx: &str,
    fam: &model::Family,
    res: &model::Res,
    cohort_reqs: Option<&String>,
    fonts: &[String],
) -> String {
    let mut s = String::new();
    s.push_str("# gflib-build provenance -- embedded build manifest (see build-fix-provenance.md)\n");
    s.push_str(&format!("family: {}\n", slug));
    s.push_str(&format!("package: {}\n", pkg));
    s.push_str(&format!("license: {}\n", spdx));
    s.push_str("source:\n");
    s.push_str(&format!("  repo: {}\n", fam.url));
    s.push_str(&format!("  commit: {}\n", fam.commit));
    let cfgp = if !fam.config_yaml.is_empty() {
        fam.config_yaml.clone()
    } else if fam.has_config {
        "(local config.yaml override)".to_string()
    } else {
        "(none)".to_string()
    };
    s.push_str(&format!("  config: {}\n", cfgp));
    s.push_str("  fetched_from: local repo archive mirror at the pinned commit\n");
    s.push_str("toolchain:\n");
    s.push_str(&format!("  backend: {}\n", res.backend));
    s.push_str(&format!("  compiler_version: {}\n", res.compiler_version));
    s.push_str(&format!("  builder: {}\n", res.builder));
    s.push_str(&format!("  builder_version: {}\n", res.builder_version));
    s.push_str("cohort:\n");
    s.push_str(&format!("  key: {}\n", res.cohort));
    s.push_str("  requirements: |\n");
    match cohort_reqs {
        Some(req) if !req.trim().is_empty() => {
            for line in req.lines() {
                s.push_str(&format!("    {}\n", line));
            }
        }
        _ => s.push_str("    (none recorded)\n"),
    }
    s.push_str("fonts:\n");
    for f in fonts {
        s.push_str(&format!("  - {}\n", f));
    }
    s.push_str("system_packages: []   # scenario B -- to be captured (auto-detect -> confirm)\n");
    s
}

// ---- dependency-free civil-date formatting (Howard Hinnant's algorithm) ----

/// (year, month, day, hour, minute, second, day-of-week with 0=Sunday) for a UTC epoch.
fn civil(epoch: f64) -> (i64, u32, u32, u32, u32, u32, i64) {
    let secs = epoch as i64;
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (h, mi, se) = ((tod / 3600) as u32, ((tod % 3600) / 60) as u32, (tod % 60) as u32);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let dow = (days.rem_euclid(7) + 4).rem_euclid(7); // 1970-01-01 was a Thursday
    (y, m, d, h, mi, se, dow)
}

fn ymd(epoch: f64) -> String {
    let (y, m, d, _, _, _, _) = civil(epoch);
    format!("{:04}{:02}{:02}", y, m, d)
}

fn rfc2822(epoch: f64) -> String {
    let (y, m, d, h, mi, se, dow) = civil(epoch);
    const DOW: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
        DOW[dow as usize],
        d,
        MON[(m - 1) as usize],
        y,
        h,
        mi,
        se,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_prefix_maps() {
        assert_eq!(license_for("ofl/x").0, "OFL-1.1");
        assert_eq!(license_for("apache/y").0, "Apache-2.0");
        assert_eq!(license_for("ufl/z").0, "UFL-1.0");
        assert!(license_for("googlefonts/w").2); // unknown prefix -> fallback flagged
        assert!(!license_for("ofl/x").2);
    }

    #[test]
    fn package_names_sanitize() {
        assert_eq!(pkg_name("ofl/oswald"), "fonts-gf-oswald");
        assert_eq!(pkg_name("apache/roboto_slab"), "fonts-gf-roboto-slab");
    }

    #[test]
    fn short_commit_caps() {
        assert_eq!(short_commit("abc1234deadbeef"), "abc1234");
        assert_eq!(short_commit(""), "0000000");
    }

    #[test]
    fn rejects_license_placeholder_holder() {
        assert!(is_license_placeholder("Copyright [yyyy] [name of copyright owner]"));
        assert!(!is_license_placeholder(
            "Copyright 2011 The ABeeZee Project Authors, with Reserved Font Name 'ABeeZee'"
        ));
    }

    #[test]
    fn civil_date_known_epoch() {
        // 1700000000 = Tue, 14 Nov 2023 22:13:20 UTC
        assert_eq!(ymd(1700000000.0), "20231114");
        assert_eq!(rfc2822(1700000000.0), "Tue, 14 Nov 2023 22:13:20 +0000");
    }

    #[test]
    fn version_string_shape() {
        let v = format!("0~gf{}.g{}-1", ymd(1700000000.0), short_commit("deadbeefcafe"));
        assert_eq!(v, "0~gf20231114.gdeadbee-1");
    }
}
