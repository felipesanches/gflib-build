//! `--export-deb`: draft a Debian source-package tree (`debian/`) for every successfully-built
//! family, so gflib-build's results can feed the self-hosted complementary apt repo. Read-only
//! w.r.t. the archive and the build outputs; only writes under `<build_dir>/packaging/`.
//! See `docs/debian-packaging-plan.md`.
//!
//! A family is drafted only when it BOTH (a) has `status=="built"` in state.json AND (b) still has
//! >=1 `.ttf`/`.otf` on disk under `<build_dir>/out/<logname>/` — out dirs are deleted after a build
//! that ran without `--keep-fonts`, so "built" alone does NOT imply fonts are present.

use crate::{config, discover, model, persist, util};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const MAINTAINER: &str = "Google Fonts Deb-Packaging (gflib-build) <juca@members.fsf.org>";

pub fn run_export_deb(cfg: &config::Config) {
    // A live daemon now auto-packages (the package worker writes packaging/ incrementally). This
    // one-shot wipes packaging/ and would clobber/race it, so refuse when a daemon owns the build dir.
    if let Some(pid) = persist::read_daemon_pid(&cfg.build_dir) {
        eprintln!(
            "a build daemon (pid {}) owns {} and already auto-packages built families.\n\
             Stop it (gflib-build --stop) or use a separate --build-dir before running --export-deb.",
            pid,
            cfg.build_dir.display()
        );
        return;
    }
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
    let by_slug: std::collections::BTreeMap<&str, &model::Family> =
        fams.iter().map(|f| (f.slug.as_str(), f)).collect();

    // Re-runs must be idempotent: wipe the packaging tree so stale dirs from a previous, larger
    // run never linger (the index would otherwise diverge from disk). It is pure generated output
    // under the build dir; fully reproducible from state + sources.
    let pkg_root = cfg.build_dir.join("packaging");
    let _ = std::fs::remove_dir_all(&pkg_root);

    let gf = cfg.google_fonts.clone();
    let mut index: Vec<serde_json::Value> = Vec::new();
    let (mut exported, mut no_fam, mut no_fonts, mut lic_fallback) = (0usize, 0usize, 0usize, 0usize);

    // --build-debs: assemble + validate a binary .deb per drafted family (a repack of the
    // already-built fonts via dpkg-deb; the from-source clean-room build is a later stage).
    let build_debs = cfg.build_debs;
    let lint_present = on_path("lintian");
    let pool = pkg_root.join("pool");
    let mut build_results = serde_json::Map::new();
    let (mut debs_built, mut debs_failed) = (0usize, 0usize);

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
        let o = package_one_family(
            &cfg.build_dir,
            slug,
            res,
            fam,
            gf.as_deref(),
            st.cohort_reqs.get(&res.cohort),
            build_debs,
            lint_present,
        );
        match o.skipped {
            Some("no_fonts") => {
                no_fonts += 1;
                continue;
            }
            Some(_) => continue, // mkdir/write failure: skip silently (a half-written tree is not a package)
            None => {}
        }
        if o.license_fallback {
            lic_fallback += 1;
        }
        if let Some(d) = o.deb_result {
            if o.deb_built {
                debs_built += 1
            } else {
                debs_failed += 1
            }
            build_results.insert(slug.clone(), d);
        }
        if let Some(e) = o.index_entry {
            index.push(e);
            exported += 1;
        }
    }

    let _ = std::fs::create_dir_all(&pkg_root);
    let doc = serde_json::json!({ "schema_version": 1, "count": index.len(), "packages": index });
    if let Ok(txt) = serde_json::to_string_pretty(&doc) {
        let _ = std::fs::write(pkg_root.join("index.json"), txt);
    }
    if build_debs {
        let bdoc = serde_json::json!({
            "schema_version": 1, "built": debs_built, "failed": debs_failed, "results": build_results,
        });
        if let Ok(txt) = serde_json::to_string_pretty(&bdoc) {
            let _ = std::fs::write(pkg_root.join("build-results.json"), txt);
        }
    }
    eprintln!("export-deb: drafted {} package(s) under {}", exported, pkg_root.display());
    eprintln!(
        "  skipped: {} built-but-not-in-worklist, {} built-but-no-fonts-on-disk (pruned without --keep-fonts){}",
        no_fam,
        no_fonts,
        if lic_fallback > 0 {
            format!(", {} license-ASSUMED OFL-1.1 (prefix not ofl/ufl/apache -> VERIFY)", lic_fallback)
        } else {
            String::new()
        }
    );
    if build_debs {
        eprintln!(
            "  debs: {} built, {} failed -> {}{}",
            debs_built,
            debs_failed,
            pool.display(),
            if lint_present { "" } else { "  (lintian absent — validated via dpkg-deb only)" }
        );
    }
}

/// What packaging one family produced — shared by the `--export-deb` one-shot and the live packaging
/// worker so both draft identical trees and build identical `.deb`s.
pub struct PackageOutcome {
    pub index_entry: Option<serde_json::Value>,
    pub deb_result: Option<serde_json::Value>, // build-results.json entry (only when build_debs)
    pub deb_built: bool,
    pub license_fallback: bool,
    pub skipped: Option<&'static str>, // "no_fonts" | "mkdir_fail" | "write_fail"
}

/// Draft the `debian/` tree for one built family under `<build_dir>/packaging/<logname>/` and, when
/// `build_debs`, assemble+validate its binary `.deb` into `packaging/pool/`. Pure generated output;
/// does NOT wipe anything (the incremental worker calls this per family). Returns what was produced.
pub fn package_one_family(
    build_dir: &Path,
    slug: &str,
    res: &model::Res,
    fam: &model::Family,
    gf: Option<&Path>,
    cohort_req: Option<&String>,
    build_debs: bool,
    lint_present: bool,
) -> PackageOutcome {
    let pkg_root = build_dir.join("packaging");
    let pool = pkg_root.join("pool");
    let mut out = PackageOutcome {
        index_entry: None,
        deb_result: None,
        deb_built: false,
        license_fallback: false,
        skipped: None,
    };

    // gate on on-disk font presence (out dir may have been pruned post-build)
    let out_dir = build_dir.join("out").join(util::slug_to_logname(slug));
    let fonts = collect_fonts(&out_dir);
    if fonts.is_empty() {
        out.skipped = Some("no_fonts");
        return out;
    }
    // De-duplicate by basename: `--backend both` writes the SAME basename under out/.../fontc/
    // and out/.../fontmake/, so a naive walk would list every font twice.
    let mut font_names: Vec<String> = fonts
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect();
    let mut seen = BTreeSet::new();
    font_names.retain(|n| seen.insert(n.clone()));
    let has_ttf = font_names.iter().any(|n| n.to_ascii_lowercase().ends_with(".ttf"));
    let has_otf = font_names.iter().any(|n| n.to_ascii_lowercase().ends_with(".otf"));

    let (spdx, dep5, fallback) = license_for(slug);
    out.license_fallback = fallback;
    let pkg = pkg_name(slug);
    let short = short_commit(&fam.commit);
    // Deterministic: a missing build timestamp (ended==0) yields 1970-01-01 rather than churning
    // the version on every run via wall-clock now().
    let epoch = res.ended.max(0.0);
    let version = format!("0~gf{}.g{}-1", ymd(epoch), short);

    let debian = pkg_root.join(util::slug_to_logname(slug)).join("debian");
    if std::fs::create_dir_all(debian.join("source")).is_err() {
        out.skipped = Some("mkdir_fail");
        return out;
    }
    let rules_path = debian.join("rules");
    let holder = copyright_holder(gf, slug);

    // Write every file, checking each result; on ANY failure skip the family entirely (do not index
    // it — a half-written tree is not a real package).
    let writes: Vec<(PathBuf, String)> = vec![
        (debian.join("control"), control(&pkg, fam, spdx)),
        (rules_path.clone(), rules(&pkg, has_ttf, has_otf)),
        (debian.join("copyright"), copyright(fam, dep5, &holder)),
        (debian.join("changelog"), changelog(&pkg, &version, slug, &fam.commit, epoch)),
        (debian.join("watch"), watch(&fam.url)),
        (debian.join("source/format"), "3.0 (quilt)\n".to_string()),
        (
            debian.join("gflib-provenance"),
            provenance(slug, &pkg, spdx, fallback, fam, res, cohort_req, &font_names),
        ),
    ];
    for (p, content) in &writes {
        if std::fs::write(p, content).is_err() {
            out.skipped = Some("write_fail");
            return out;
        }
    }
    set_exec(&rules_path);

    if build_debs {
        let pkg_dir = pkg_root.join(util::slug_to_logname(slug));
        let r = build_one_deb(&pkg_dir, &pool, &pkg, &version, fam, &fonts, lint_present);
        out.deb_built = r.built;
        let mut dr = serde_json::json!({
            "built": r.built, "validated": r.validated, "deb_bytes": r.deb_bytes,
            "lint": r.lint, "error": r.error, "package": pkg, "version": version,
        });
        // only record lint_tags when lintian actually ran — its absence marks a package as still
        // needing a (re-)lint, so the retroactive pass can tell "clean (no tags)" from "not yet linted".
        if r.lint_ran {
            dr["lint_tags"] = serde_json::json!(tags_json(&r.lint_tags));
        }
        out.deb_result = Some(dr);
    }
    out.index_entry = Some(serde_json::json!({
        "slug": slug, "package": pkg, "version": version, "license": spdx,
        "license_assumed": fallback, "fonts": font_names.len(), "backend": res.backend,
        "compiler_version": res.compiler_version, "deb_built": out.deb_built,
    }));
    out
}

// ---- helpers ----

/// Recursively collect .ttf/.otf files under `dir` (out/<logname>/ may nest fontc/ + fontmake/).
/// Uses `file_type()` (no symlink follow) so a stray symlink can't send the walk on an infinite
/// upward chase.
fn collect_fonts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
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
            let path = ent.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let e = ext.to_ascii_lowercase();
                    if e == "ttf" || e == "otf" {
                        out.push(path);
                    }
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
        _ => ("OFL-1.1", "OFL-1.1", true), // dominant GF license; flagged + marked for review
    }
}

/// Debian binary package name `fonts-gf-<family>` (lowercase, [a-z0-9.+-]; must start AND end alnum).
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
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    while s.chars().next().map(|c| !c.is_ascii_alphanumeric()).unwrap_or(false) {
        s.remove(0);
    }
    while s.chars().last().map(|c| !c.is_ascii_alphanumeric()).unwrap_or(false) {
        s.pop();
    }
    if s.is_empty() {
        // all-symbol / non-ASCII family name (not producible from real ASCII GF/GitHub slugs, but
        // never emit the invalid "fonts-gf-"): a deterministic, collision-resistant token.
        let h = slug.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
        s = format!("x{:08x}", h);
    }
    format!("fonts-gf-{}", s)
}

fn short_commit(c: &str) -> String {
    let s: String = c.chars().take(7).collect(); // char-safe (never splits a UTF-8 boundary)
    if s.is_empty() {
        "0000000".to_string()
    } else {
        s
    }
}

/// Collapse to a single safe line (defensive: control/copyright fields must not contain raw CR/LF).
fn oneline(s: &str) -> String {
    s.replace(['\r', '\n'], " ").trim().to_string()
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
        // OFL.txt/UFL.txt carry the real holder on their first "Copyright" line(s); the Apache
        // LICENSE is the license TEXT (placeholder "Copyright [yyyy] [name of copyright owner]"),
        // and a bare OFL license body has the template "Copyright (c) <dates>, <Copyright Holder>".
        // Skip placeholder lines; join the copyright's continuation lines (the "with Reserved Font
        // Name" clause) so it is not truncated.
        for lf in ["OFL.txt", "UFL.txt", "NOTICE", "LICENSE.txt", "LICENSE"] {
            if let Ok(raw) = std::fs::read_to_string(gf.join(slug).join(lf)) {
                let txt = raw.strip_prefix('\u{feff}').unwrap_or(&raw); // strip UTF-8 BOM
                let lines: Vec<&str> = txt.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    let t = line.trim();
                    if t.starts_with("Copyright") && !is_license_placeholder(t) {
                        let mut parts = vec![t.to_string()];
                        for nxt in lines[i + 1..].iter().take(2) {
                            let n = nxt.trim();
                            if n.is_empty() {
                                break;
                            }
                            parts.push(n.to_string());
                        }
                        return oneline(parts.join(" ").trim_end_matches(',').trim());
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
    l.contains("[yyyy]")
        || l.contains("[name of copyright owner]")
        || l.contains("[year]")
        || (l.contains('<') && l.contains('>')) // <dates>, <Copyright Holder>, <URL|email>, <year>
}

fn control(pkg: &str, fam: &model::Family, spdx: &str) -> String {
    // Built line-by-line (NOT via `\n\` continuation, which strips the leading whitespace that
    // control-file field folding and the long Description require).
    let name = oneline(if fam.name.is_empty() { pkg } else { fam.name.as_str() });
    let url = oneline(&fam.url);
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
    if !url.is_empty() {
        v.push(format!("Homepage: {}", url));
        v.push(format!("Vcs-Browser: {}", url));
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
    let name = oneline(&fam.name);
    let url = oneline(&fam.url);
    let holder = oneline(holder);
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
        name = name,
        url = url,
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
    let url = oneline(url);
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

#[allow(clippy::too_many_arguments)]
fn provenance(
    slug: &str,
    pkg: &str,
    spdx: &str,
    license_assumed: bool,
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
    if license_assumed {
        s.push_str("# WARNING: license ASSUMED -- slug prefix is not ofl/ufl/apache; VERIFY against upstream\n");
        s.push_str("license_assumed: true\n");
    }
    s.push_str("source:\n");
    s.push_str(&format!("  repo: {}\n", oneline(&fam.url)));
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

// ---- binary .deb assembly (repack of the built fonts via dpkg-deb; from-source build is later) ----

#[derive(Default)]
struct DebResult {
    built: bool,
    validated: bool,
    deb_bytes: u64,
    lint: String,
    lint_ran: bool,                  // lintian actually executed (distinguishes "clean, no tags" from "not run")
    lint_tags: Vec<(char, String)>,  // distinct (severity, tag) findings, for category grouping
    error: String,
}

/// Assemble + validate a binary .deb for one family from its built fonts. Stages a tree under
/// `pkg_dir/_build/`, runs `dpkg-deb --root-owner-group --build` into `pool/`, then validates with
/// `dpkg-deb --info`/`--contents` (+ `lintian` when present).
fn build_one_deb(
    pkg_dir: &Path,
    pool: &Path,
    pkg: &str,
    version: &str,
    fam: &model::Family,
    fonts: &[PathBuf],
    lint: bool,
) -> DebResult {
    let mut res = DebResult::default();
    let famn = pkg.strip_prefix("fonts-gf-").unwrap_or(pkg);
    let stage = pkg_dir.join("_build");
    let _ = std::fs::remove_dir_all(&stage);

    // copy the fonts (de-duplicated by basename) into /usr/share/fonts/{truetype,opentype}/gf-<fam>/
    let mut seen = BTreeSet::new();
    let mut size = 0u64;
    for f in fonts {
        let name = match f.file_name().map(|n| n.to_string_lossy().to_string()) {
            Some(n) => n,
            None => continue,
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        let sub = if name.to_ascii_lowercase().ends_with(".otf") { "opentype" } else { "truetype" };
        let dst_dir = stage.join("usr/share/fonts").join(sub).join(format!("gf-{}", famn));
        if std::fs::create_dir_all(&dst_dir).is_err() {
            res.error = "stage mkdir failed".into();
            return res;
        }
        let dst = dst_dir.join(&name);
        if std::fs::copy(f, &dst).is_err() {
            res.error = format!("copy {} failed", name);
            return res;
        }
        size += std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    }
    if seen.is_empty() {
        res.error = "no fonts to package".into();
        return res;
    }

    // DEBIAN/control (binary control)
    let ctrl_dir = stage.join("DEBIAN");
    if std::fs::create_dir_all(&ctrl_dir).is_err() {
        res.error = "DEBIAN mkdir failed".into();
        return res;
    }
    let installed_kb = size.div_ceil(1024).max(1);
    if std::fs::write(ctrl_dir.join("control"), binary_control(pkg, version, fam, installed_kb)).is_err() {
        res.error = "write DEBIAN/control failed".into();
        return res;
    }

    // build the .deb
    let _ = std::fs::create_dir_all(pool);
    // drop any prior .deb(s) for this package (a different version, e.g. a rebuild on a later date) so
    // pool/ keeps at most the current one — the live worker never wipes packaging/.
    let prefix = format!("{}_", pkg);
    if let Ok(rd) = std::fs::read_dir(pool) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) && name.ends_with("_all.deb") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let deb_path = pool.join(format!("{}_{}_all.deb", pkg, version));
    match std::process::Command::new("dpkg-deb")
        .args(["--root-owner-group", "--build"])
        .arg(&stage)
        .arg(&deb_path)
        .output()
    {
        Ok(o) if o.status.success() => res.built = true,
        Ok(o) => {
            res.error = format!("dpkg-deb: {}", String::from_utf8_lossy(&o.stderr).trim());
            let _ = std::fs::remove_dir_all(&stage);
            return res;
        }
        Err(e) => {
            res.error = format!("dpkg-deb spawn: {}", e);
            return res;
        }
    }
    res.deb_bytes = std::fs::metadata(&deb_path).map(|m| m.len()).unwrap_or(0);

    // validate: control parses, and the archive actually contains the fonts
    let info_ok = std::process::Command::new("dpkg-deb")
        .arg("--info")
        .arg(&deb_path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let has_font = std::process::Command::new("dpkg-deb")
        .arg("--contents")
        .arg(&deb_path)
        .output()
        .map(|o| {
            let t = String::from_utf8_lossy(&o.stdout);
            t.contains(".ttf") || t.contains(".otf")
        })
        .unwrap_or(false);
    res.validated = info_ok && has_font;

    // the full lintian output is saved next to the .deb so the UI can show the report on demand
    let report_path = pool.join(format!("{}_{}.lintian.txt", pkg, version));
    res.lint = if lint {
        match run_lintian(&deb_path, &report_path) {
            Some((s, tags)) => {
                res.lint_ran = true;
                res.lint_tags = tags;
                s
            }
            None => {
                let _ = std::fs::remove_file(&report_path);
                "lintian failed to run".into()
            }
        }
    } else {
        let _ = std::fs::remove_file(&report_path); // stale report from a prior run where lintian was present
        "not run (lintian absent)".into()
    };

    let _ = std::fs::remove_dir_all(&stage); // keep the .deb in pool/, drop the staging tree
    res
}

/// The binary-package DEBIAN/control (distinct from the source debian/control).
fn binary_control(pkg: &str, version: &str, fam: &model::Family, installed_kb: u64) -> String {
    let name = oneline(if fam.name.is_empty() { pkg } else { fam.name.as_str() });
    let v: Vec<String> = vec![
        format!("Package: {}", pkg),
        format!("Version: {}", version),
        "Architecture: all".into(),
        format!("Maintainer: {}", MAINTAINER),
        format!("Installed-Size: {}", installed_kb),
        "Section: fonts".into(),
        "Priority: optional".into(),
        "Multi-Arch: foreign".into(),
        format!("Description: {} -- Google Fonts, reproducible build", name),
        " A repack of the gflib-build reproducible build (the from-source clean-room build is a".into(),
        " later stage). The source package's debian/gflib-provenance records the exact recipe.".into(),
    ];
    v.join("\n") + "\n"
}

// ---- package metadata (the control INSIDE the built .deb, + the source recipe) ----

/// Human-readable metadata for one family's package: the binary .deb's own control + installed
/// files (via dpkg-deb), then the source debian/ recipe (control with Build-Depends, provenance, …).
/// Used by both UIs so the package detail view is identical.
pub fn package_metadata(build_dir: &Path, slug: &str) -> String {
    let pkg_root = build_dir.join("packaging");
    let mut out = String::new();
    match find_built_deb(&pkg_root, slug) {
        Some(p) => {
            let fname = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            out.push_str(&format!("══ binary package: {} ══\n\n", fname));
            out.push_str("# control (the DEBIAN/control inside the .deb)  ·  dpkg-deb --info\n");
            out.push_str(&dpkg_deb("--info", &p));
            out.push_str("\n# installed files  ·  dpkg-deb --contents\n");
            out.push_str(&dpkg_deb("--contents", &p));
            out.push('\n');
        }
        None => out.push_str(
            "══ binary package ══\n(not built yet — enable 'build .deb packages' in the config tab; \
             the daemon then auto-packages built families)\n\n",
        ),
    }
    out.push_str("══ lintian report ══\n");
    out.push_str(&lintian_report(build_dir, slug));
    out.push('\n');
    let dpath = pkg_root.join(slug.replace('/', "__")).join("debian");
    for f in ["control", "gflib-provenance", "changelog", "copyright", "rules", "watch"] {
        out.push_str(&format!("══ source debian/{} ══\n", f));
        match std::fs::read_to_string(dpath.join(f)) {
            Ok(t) => {
                out.push_str(&t);
                if !t.ends_with('\n') {
                    out.push('\n');
                }
            }
            Err(_) => out.push_str("(not drafted — run --export-deb)\n"),
        }
        out.push('\n');
    }
    out
}

/// Parse a lintian output line `E: pkg: tag args…` (or `W: …`) into (severity, tag). Other lines → None.
fn lint_line_tag(line: &str) -> Option<(char, String)> {
    let sev = line.as_bytes().first().copied()? as char;
    if sev != 'E' && sev != 'W' {
        return None;
    }
    let rest = line.strip_prefix("E: ").or_else(|| line.strip_prefix("W: "))?;
    let after_pkg = rest.splitn(2, ": ").nth(1)?; // drop the "pkg: " prefix
    let tag = after_pkg.split_whitespace().next()?;
    Some((sev, tag.to_string()))
}

/// Run lintian on `deb`, save the full report to `report_path`, and return (summary, distinct findings).
/// summary is "clean" | "N warnings" | "N errors, M warnings"; findings are distinct (severity, tag)
/// pairs (a package counts once per tag) for category grouping. None if lintian could not be spawned.
fn run_lintian(deb: &Path, report_path: &Path) -> Option<(String, Vec<(char, String)>)> {
    let o = std::process::Command::new("lintian").arg(deb).output().ok()?;
    let txt = String::from_utf8_lossy(&o.stdout);
    let body = if txt.trim().is_empty() {
        "# lintian: no findings — clean.\n".to_string()
    } else {
        txt.to_string()
    };
    let _ = std::fs::write(report_path, body.as_bytes());
    let e = txt.lines().filter(|l| l.starts_with("E:")).count();
    let w = txt.lines().filter(|l| l.starts_with("W:")).count();
    let mut tags: Vec<(char, String)> = Vec::new();
    for l in txt.lines() {
        if let Some(t) = lint_line_tag(l) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
    }
    let summary = if e > 0 {
        format!("{} errors, {} warnings", e, w)
    } else if w > 0 {
        format!("{} warnings", w)
    } else {
        "clean".into()
    };
    Some((summary, tags))
}

/// (severity, tag) pairs as JSON-friendly [sev, tag] string arrays.
fn tags_json(tags: &[(char, String)]) -> Vec<[String; 2]> {
    tags.iter().map(|(s, t)| [s.to_string(), t.clone()]).collect()
}

/// Retroactively lint an already-built .deb (no rebuild): runs lintian on `pool/<pkg>_<ver>_all.deb`,
/// saves the report, and returns (summary, [sev,tag] findings). None if the .deb is missing or lintian
/// can't be spawned. Lets a freshly-installed lintian cover the whole backlog of already-validated packages.
pub fn relint_deb(pool: &Path, pkg: &str, version: &str) -> Option<(String, Vec<[String; 2]>)> {
    let deb = pool.join(format!("{}_{}_all.deb", pkg, version));
    if !deb.is_file() {
        return None;
    }
    let report_path = pool.join(format!("{}_{}.lintian.txt", pkg, version));
    run_lintian(&deb, &report_path).map(|(s, tags)| (s, tags_json(&tags)))
}

/// Public: the built .deb path for a slug (for download), or None if not built.
pub fn deb_file(build_dir: &Path, slug: &str) -> Option<PathBuf> {
    find_built_deb(&build_dir.join("packaging"), slug)
}

/// Public: the saved lintian report for a slug, or a friendly placeholder if none is on file.
pub fn lintian_report(build_dir: &Path, slug: &str) -> String {
    let pkg_root = build_dir.join("packaging");
    let found = (|| {
        let txt = std::fs::read_to_string(pkg_root.join("build-results.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
        let r = v.get("results")?.get(slug)?;
        let pkg = r.get("package")?.as_str()?;
        let ver = r.get("version")?.as_str()?;
        std::fs::read_to_string(pkg_root.join("pool").join(format!("{}_{}.lintian.txt", pkg, ver))).ok()
    })();
    found.unwrap_or_else(|| {
        "(no lintian report on file — lintian may not have run for this package, or it predates report \
         capture. Enable 'lintian' in the toolchain and rebuild the .deb to generate one.)\n"
            .into()
    })
}

/// Locate the built .deb in pool/ for a slug, via packaging/build-results.json (package + version).
fn find_built_deb(pkg_root: &Path, slug: &str) -> Option<PathBuf> {
    let txt = std::fs::read_to_string(pkg_root.join("build-results.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let r = v.get("results")?.get(slug)?;
    if !r.get("built").and_then(|b| b.as_bool()).unwrap_or(false) {
        return None;
    }
    let pkg = r.get("package")?.as_str()?;
    let ver = r.get("version")?.as_str()?;
    let p = pkg_root.join("pool").join(format!("{}_{}_all.deb", pkg, ver));
    p.is_file().then_some(p)
}

fn dpkg_deb(flag: &str, deb: &Path) -> String {
    match std::process::Command::new("dpkg-deb").arg(flag).arg(deb).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => format!("(dpkg-deb {} failed: {})\n", flag, String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => format!("(dpkg-deb spawn failed: {})\n", e),
    }
}

// ---- deb-build external toolchain detection (auto-recovers as tools appear) ----

/// Required external programs for deb building/validation: (program, apt-package, purpose).
const DEB_TOOLS: [(&str, &str, &str); 5] = [
    ("dpkg-deb", "dpkg", "assemble binary .deb packages"),
    ("dpkg-buildpackage", "dpkg-dev", "build source packages"),
    ("fakeroot", "fakeroot", "fake root for correct file ownership"),
    ("dh", "debhelper", "the dh sequencer (dh $@) for source builds"),
    ("lintian", "lintian", "package validation / policy checks"),
];

/// Is `prog` an executable on PATH? Scans PATH directly — no subprocess.
pub fn on_path(prog: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let path = match std::env::var("PATH") {
        Ok(p) => p,
        Err(_) => return false,
    };
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let p = Path::new(dir).join(prog);
        if let Ok(md) = std::fs::metadata(&p) {
            if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                return true;
            }
        }
    }
    false
}

/// Detect the deb toolchain now (a PATH scan).
pub fn detect_deb_tools() -> Vec<model::DebTool> {
    DEB_TOOLS
        .iter()
        .map(|(name, provides, purpose)| model::DebTool {
            name: name.to_string(),
            present: on_path(name),
            provides: provides.to_string(),
            purpose: purpose.to_string(),
        })
        .collect()
}

/// Detect with a 5-second cache, so the snapshot can show the toolchain cheaply AND recover within
/// ~5s of a missing tool being installed — no restart required.
pub fn deb_tools_cached() -> Vec<model::DebTool> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<(f64, Vec<model::DebTool>)>> = OnceLock::new();
    let m = CACHE.get_or_init(|| Mutex::new((0.0, Vec::new())));
    let mut g = m.lock().unwrap();
    if g.1.is_empty() || util::now() - g.0 > 5.0 {
        g.1 = detect_deb_tools();
        g.0 = util::now();
    }
    g.1.clone()
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
    fn lintian_line_tag_parsing() {
        assert_eq!(lint_line_tag("E: fonts-gf-kosugi: no-copyright-file"), Some(('E', "no-copyright-file".into())));
        // tag followed by args (path, parenthetical) -> just the tag
        assert_eq!(
            lint_line_tag("E: fonts-gf-robotoslab: no-changelog usr/share/doc/fonts-gf-robotoslab/changelog.Debian.gz (non-native package)"),
            Some(('E', "no-changelog".into()))
        );
        assert_eq!(lint_line_tag("W: pkg: spelling-error-in-description foo bar"), Some(('W', "spelling-error-in-description".into())));
        // non-finding lines are ignored
        assert_eq!(lint_line_tag("N: 1 hint"), None);
        assert_eq!(lint_line_tag(""), None);
        assert_eq!(lint_line_tag("I: pkg: some-info"), None); // only E/W are findings we group
    }

    #[test]
    fn package_names_sanitize() {
        assert_eq!(pkg_name("ofl/oswald"), "fonts-gf-oswald");
        assert_eq!(pkg_name("apache/roboto_slab"), "fonts-gf-roboto-slab");
        assert_eq!(pkg_name("ofl/abc-"), "fonts-gf-abc"); // trailing separator stripped
        assert_eq!(pkg_name("ofl/a--b"), "fonts-gf-a-b"); // runs collapsed
        assert!(pkg_name("ofl/日本語").starts_with("fonts-gf-x")); // non-empty deterministic token
        assert_ne!(pkg_name("ofl/日本語"), pkg_name("ofl/한국어")); // no collision
    }

    #[test]
    fn short_commit_caps() {
        assert_eq!(short_commit("abc1234deadbeef"), "abc1234");
        assert_eq!(short_commit(""), "0000000");
        assert_eq!(short_commit("café567"), "café567".chars().take(7).collect::<String>()); // no panic
    }

    #[test]
    fn rejects_license_placeholder_holder() {
        assert!(is_license_placeholder("Copyright [yyyy] [name of copyright owner]"));
        assert!(is_license_placeholder("Copyright (c) <dates>, <Copyright Holder> (<URL|email>)"));
        assert!(!is_license_placeholder(
            "Copyright 2011 The ABeeZee Project Authors, with Reserved Font Name 'ABeeZee'"
        ));
    }

    #[test]
    fn civil_date_known_epoch() {
        // 1700000000 = Tue, 14 Nov 2023 22:13:20 UTC
        assert_eq!(ymd(1700000000.0), "20231114");
        assert_eq!(rfc2822(1700000000.0), "Tue, 14 Nov 2023 22:13:20 +0000");
        assert_eq!(ymd(0.0), "19700101"); // deterministic when ended==0
    }

    #[test]
    fn binary_control_fields() {
        let fam = model::Family { name: "Test Family".into(), ..Default::default() };
        let c = binary_control("fonts-gf-test", "0~gf20231114.gabc1234-1", &fam, 12);
        assert!(c.contains("Package: fonts-gf-test"));
        assert!(c.contains("Version: 0~gf20231114.gabc1234-1"));
        assert!(c.contains("Architecture: all"));
        assert!(c.contains("Installed-Size: 12"));
        assert!(c.contains("Section: fonts"));
    }

    #[test]
    fn deb_tools_listed() {
        let t = detect_deb_tools();
        assert_eq!(t.len(), 5);
        assert!(t.iter().any(|x| x.name == "lintian" && x.provides == "lintian"));
        assert!(t.iter().any(|x| x.name == "dpkg-buildpackage" && x.provides == "dpkg-dev"));
        assert!(t.iter().any(|x| x.name == "dh" && x.provides == "debhelper"));
    }

    #[test]
    fn version_string_shape() {
        let v = format!("0~gf{}.g{}-1", ymd(1700000000.0), short_commit("deadbeefcafe"));
        assert_eq!(v, "0~gf20231114.gdeadbee-1");
    }
}
