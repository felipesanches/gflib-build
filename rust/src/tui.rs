//! The curses-style dashboard, ported to crossterm. Renders the same snapshot the Python TUI does:
//! a two-line header (cumulative elapsed + disk/jobs/cohorts/backends), a segmented phase/progress
//! bar, Tab/Shift-Tab tabs (config · overview · queue · cohorts · archive · built · failures · stats),
//! a stack of sections per tab (←/→ focuses a section, ↑/↓ moves within it, ↵ opens a detail
//! overlay), an always-on status panel, and a footer. Live controls (pause, retry, jobs/percent) are
//! written to control.json — the same channel the web UI uses — so a live build and an attached
//! monitor behave identically.

use crate::model::{ControlSet, Snapshot};
use crate::monitor::Source;
use crate::util::{human, hms};
use crossterm::event::{self, Event, KeyCode};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Stdout, Write};
use std::sync::Arc;
use std::time::Duration;

// Tab order MUST match the web UI's TABS (a user switching between the terminal and the browser
// sees the same tabs in the same order).
const TABS: [&str; 13] = [
    "config", "overview", "queue", "cohorts", "archive", "built", "packaging", "tools", "failures", "stats", "fontspector", "crater", "reset",
];

/// Succinct glossary of the Python->Rust migration milestones, shown in the tools tab so a UI label
/// like "M5 blocker" is self-documenting. Keep in sync with web.rs MILESTONES + docs/migration-milestones.md.
const MILESTONES: [(&str, &str); 8] = [
    ("M0", "Measurement foundation — record compiler + exact version for every build attempt"),
    ("M1", "Full buildability — 100% of buildable families produce the expected fonts (any backend)"),
    ("M2", "fontc-gap map — every buildable family attempted with fontc, the result recorded"),
    ("M3", "fontc equivalence — fontc output equivalent to fontmake/shipped, at scale"),
    ("M4", "fontc majority — families that build correctly with fontc alone (no fontmake fallback)"),
    ("M5", "Python-free pipeline — Rust-native gftools-builder3, no Python pre-build or deps"),
    ("M6", "latest-fontc currency — the M4/M5 set re-validated on the latest fontc"),
    ("M7", "100% Rust — the whole library: latest fontc, equivalent output, zero Python"),
];

/// Colour for a fontspector status: FAIL/FATAL/ERROR red · WARN yellow · PASS green · else grey.
fn fs_color(status: &str) -> Color {
    match status {
        "PASS" => Color::Green,
        "WARN" => Color::Yellow,
        "FAIL" | "FATAL" | "ERROR" => Color::Red,
        _ => Color::DarkGrey,
    }
}

/// Severity rank for sorting fontspector statuses worst-first.
fn sev_rank(s: &str) -> u8 {
    match s {
        "SKIP" => 0, "PASS" => 1, "INFO" => 2, "WARN" => 3, "FAIL" => 4, "FATAL" => 5, "ERROR" => 6, _ => 1,
    }
}

const SOURCE_CHOICES: [&str; 2] = ["metadata", "archive"];
const BACKEND_CHOICES: [&str; 4] = ["auto", "fontc", "fontmake", "both"];
const ORCHESTRATOR_CHOICES: [&str; 3] = ["auto", "builder3", "builder2"];

// ---- the unified Configuration tab: a full schema editor used for BOTH live editing and first-run
// setup — a faithful port of CONFIG_SCHEMA + the _cfg_* helpers from the original Python tool. ----
#[derive(Clone)]
enum CfgKind {
    Choice(&'static [&'static str]),
    Path,
    Bool,
    Step { step: f64, min: f64, max: f64 },
}

#[derive(Clone)]
struct CfgField {
    key: &'static str,
    label: &'static str,
    kind: CfgKind,
    live: bool,            // can change on a running build (else needs a restart: C)
    value: String,         // choice name / path or text / stepnum text (unused for Bool)
    bval: bool,            // Bool value
    caret: usize,          // text-edit caret (path / text / stepnum)
}

/// The schema in display order — (key, label, kind, live). Mirrors Python's CONFIG_SCHEMA.
fn cfg_schema() -> Vec<(&'static str, &'static str, CfgKind, bool)> {
    // ordered by group (see cfg_group) so the config tab renders related fields under one sub-header
    vec![
        // Sources & paths
        ("source", "worklist source", CfgKind::Choice(&SOURCE_CHOICES), false),
        ("google_fonts", "google/fonts clone", CfgKind::Path, false),
        ("archive", "repo archive", CfgKind::Path, false),
        ("build_dir", "build output dir", CfgKind::Path, false),
        // Build engine
        ("backend", "build backend", CfgKind::Choice(&BACKEND_CHOICES), true),
        ("orchestrator", "orchestrator", CfgKind::Choice(&ORCHESTRATOR_CHOICES), false),
        ("fontc_bin", "fontc binary (override)", CfgKind::Path, false),
        ("auto_provision", "auto-provision pinned toolchain", CfgKind::Bool, false),
        ("manage_venvs", "cohort venvs", CfgKind::Bool, false),
        ("jobs", "parallel jobs", CfgKind::Step { step: 1.0, min: 1.0, max: 256.0 }, true),
        ("timeout", "per-build timeout (0=off)", CfgKind::Step { step: 30.0, min: 0.0, max: 100000.0 }, true),
        // Scope
        ("percent", "percent of library", CfgKind::Step { step: 5.0, min: 1.0, max: 100.0 }, true),
        ("retry_failed", "retry ALL failed (incl. genuine errors)", CfgKind::Bool, false),
        ("auto_upgrade", "auto-upgrade built families (better backend)", CfgKind::Bool, false),
        ("populate_archive", "populate archive (fetch repos)", CfgKind::Bool, true),
        // QA & packaging
        ("compare", "compare to shipped", CfgKind::Bool, true),
        ("fontspector_qa", "fontspector QA on green builds", CfgKind::Bool, false),
        ("build_debs", "build .deb packages (auto-package built families)", CfgKind::Bool, true),
    ]
}

/// The config-panel sub-section a field belongs to (drives the grouped headers in render_config).
fn cfg_group(key: &str) -> &'static str {
    match key {
        "source" | "google_fonts" | "archive" | "build_dir" => "Sources & paths",
        "backend" | "orchestrator" | "fontc_bin" | "auto_provision" | "manage_venvs" | "jobs" | "timeout" => "Build engine",
        "percent" | "retry_failed" | "auto_upgrade" | "populate_archive" => "Scope",
        "compare" | "fontspector_qa" | "build_debs" => "QA & packaging",
        _ => "Other",
    }
}

/// Python `{:g}` — trim a float to the shortest exact decimal (no trailing .0 for integers).
fn fmt_g(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        let s = format!("{}", x);
        s
    }
}

/// Build editable field descriptors from a config map (port of `_cfg_init_fields`).
fn cfg_init_fields(cfg: &BTreeMap<String, Value>) -> Vec<CfgField> {
    cfg_schema().into_iter().map(|(key, label, kind, live)| {
        let v = cfg.get(key);
        let (value, bval) = match &kind {
            CfgKind::Bool => (String::new(), v.and_then(|x| x.as_bool()).unwrap_or(false)),
            CfgKind::Choice(ch) => {
                let s = v.and_then(|x| x.as_str()).unwrap_or("");
                (if ch.contains(&s) { s.to_string() } else { ch[0].to_string() }, false)
            }
            CfgKind::Step { .. } | CfgKind::Path => {
                let mut s = match v {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Number(n)) => {
                        if let Some(i) = n.as_i64() { i.to_string() }
                        else if let Some(f) = n.as_f64() { fmt_g(f) }
                        else { n.to_string() }
                    }
                    _ => String::new(),
                };
                if key == "timeout" && s.is_empty() {
                    s = "0".into();
                }
                if matches!(kind, CfgKind::Path) && !s.is_empty() {
                    s = display_path(&s);
                }
                (s, false)
            }
        };
        let caret = value.chars().count();
        CfgField { key, label, kind, live, value, bval, caret }
    }).collect()
}

/// Show a path relative to the cwd when it lives under it (port of `_display_path`).
fn display_path(p: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let abs = std::path::Path::new(p);
    if let Ok(rel) = abs.strip_prefix(&cwd) {
        return rel.display().to_string();
    }
    p.to_string()
}

/// The typed config from the current field values (port of `_cfg_typed`; timeout 0 → null).
fn cfg_typed(fields: &[CfgField]) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for f in fields {
        let v = match &f.kind {
            CfgKind::Bool => json!(f.bval),
            CfgKind::Choice(_) => json!(f.value),
            CfgKind::Step { .. } => {
                let x: f64 = f.value.trim().parse().unwrap_or(0.0);
                if x == x.trunc() { json!(x as i64) } else { json!(x) }
            }
            CfgKind::Path => json!(f.value),
        };
        out.insert(f.key.to_string(), v);
    }
    if matches!(out.get("timeout"), Some(t) if t.as_i64() == Some(0) || t.as_f64() == Some(0.0)) {
        out.insert("timeout".into(), Value::Null);
    }
    out
}

/// Field visibility (port of the schema `show_if` predicates).
fn cfg_show(key: &str, vals: &BTreeMap<String, Value>) -> bool {
    let s = |k: &str| vals.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match key {
        "google_fonts" => s("source") == "metadata",
        "fontc_bin" => s("backend") != "fontmake",
        "auto_provision" => s("fontc_bin").is_empty(), // an explicit override bypasses provisioning
        "compare" => s("source") == "metadata",
        _ => true,
    }
}

/// Indices (into `fields`) of the currently-visible fields (port of `_cfg_visible`).
fn cfg_visible(fields: &[CfgField]) -> Vec<usize> {
    let vals = cfg_typed(fields);
    (0..fields.len()).filter(|&i| cfg_show(fields[i].key, &vals)).collect()
}

/// Edit one field from a keypress (port of `_cfg_field_key`). Returns true on Enter (advance).
fn cfg_field_key(f: &mut CfgField, code: KeyCode) -> bool {
    let charcount = |s: &str| s.chars().count();
    let del_at = |s: &str, i: usize| -> String { // remove the char before index i
        s.chars().take(i.saturating_sub(1)).chain(s.chars().skip(i)).collect()
    };
    match &f.kind {
        CfgKind::Bool => {
            if matches!(code, KeyCode::Char(' ') | KeyCode::Enter) {
                f.bval = !f.bval;
            }
            false
        }
        CfgKind::Choice(ch) => {
            let ci = ch.iter().position(|c| *c == f.value).unwrap_or(0);
            match code {
                KeyCode::Char(' ') | KeyCode::Right => { f.value = ch[(ci + 1) % ch.len()].to_string(); false }
                KeyCode::Left => { f.value = ch[(ci + ch.len() - 1) % ch.len()].to_string(); false }
                KeyCode::Enter => true,
                _ => false,
            }
        }
        CfgKind::Step { step, min, max } => match code {
            KeyCode::Left | KeyCode::Right => {
                let dir = if code == KeyCode::Right { 1.0 } else { -1.0 };
                let x: f64 = f.value.trim().parse().unwrap_or(0.0);
                f.value = fmt_g((x + step * dir).clamp(*min, *max));
                f.caret = charcount(&f.value);
                false
            }
            KeyCode::Backspace => {
                if f.caret > 0 { f.value = del_at(&f.value, f.caret); f.caret -= 1; }
                false
            }
            KeyCode::Home => { f.caret = 0; false }
            KeyCode::End => { f.caret = charcount(&f.value); false }
            KeyCode::Enter => true,
            KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                let mut v: Vec<char> = f.value.chars().collect();
                v.insert(f.caret.min(v.len()), c);
                f.value = v.into_iter().collect();
                f.caret += 1;
                false
            }
            _ => false,
        },
        CfgKind::Path => match code {
            KeyCode::Left => { f.caret = f.caret.saturating_sub(1); false }
            KeyCode::Right => { f.caret = (f.caret + 1).min(charcount(&f.value)); false }
            KeyCode::Home => { f.caret = 0; false }
            KeyCode::End => { f.caret = charcount(&f.value); false }
            KeyCode::Backspace => {
                if f.caret > 0 { f.value = del_at(&f.value, f.caret); f.caret -= 1; }
                false
            }
            KeyCode::Enter => true,
            KeyCode::Char(c) if (c as u32) >= 32 => {
                let mut v: Vec<char> = f.value.chars().collect();
                v.insert(f.caret.min(v.len()), c);
                f.value = v.into_iter().collect();
                f.caret += 1;
                false
            }
            _ => false,
        },
    }
}

/// The action button labels for the config tab (setup: launch/cancel; live: apply).
fn cfg_actions(setup: bool) -> &'static [&'static str] {
    if setup {
        &["▶ Start build", "Cancel"]
    } else {
        &["✓ apply changes"]
    }
}

/// The cwd-independent settings the config tab may persist on 'apply' — a subset of save_config's
/// keys that excludes every path, so a relative display-path can never be written back to disk. This
/// also gates which non-live fields are editable on a running build: editable ⇔ appliable-live OR
/// persistable, so the UI never invites an edit that 'apply' would silently drop.
const TUI_PERSIST: &[&str] = &[
    "source", "backend", "jobs", "percent", "compare", "manage_venvs", "fontspector_qa", "build_debs",
];

fn cfg_persistable(key: &str) -> bool {
    TUI_PERSIST.contains(&key)
}

/// A config field is editable when we're in the setup wizard, OR it applies live to a running build,
/// OR it is a cwd-independent setting we can persist for the next run.
fn cfg_editable(setup: bool, f: &CfgField) -> bool {
    setup || f.live || cfg_persistable(f.key)
}

/// Live 'apply': forward the live-honoured keys (backend/jobs/percent/compare) to control.json AND
/// persist the cwd-independent subset (incl. build_debs) to gflib-build.config so a later
/// `--export-deb` / next run sees them. Returns a short status line for the config-tab footer.
fn cfg_apply_live(fields: &[CfgField], snap: &Snapshot, src: &dyn Source) -> String {
    let new = cfg_typed(fields);
    let cur = &snap.config;
    let changed = |k: &str| new.get(k) != cur.get(k);
    let mut set = ControlSet::default();
    if changed("backend") {
        if let Some(b) = new.get("backend").and_then(|v| v.as_str()) {
            set.backend = Some(b.to_string());
        }
    }
    if changed("jobs") {
        if let Some(j) = new.get("jobs").and_then(|v| v.as_i64()) {
            set.jobs = Some(j.max(1) as usize);
        }
    }
    if changed("percent") {
        if let Some(p) = new.get("percent").and_then(|v| v.as_f64()) {
            set.percent = Some(p);
        }
    }
    if changed("compare") {
        if let Some(c) = new.get("compare").and_then(|v| v.as_bool()) {
            set.compare = Some(c);
        }
    }
    if changed("build_debs") {
        if let Some(b) = new.get("build_debs").and_then(|v| v.as_bool()) {
            set.build_debs = Some(b); // live: starts/stops the auto-packaging worker
        }
    }
    src.control(&set);
    // persist the cwd-independent subset for the next run / --export-deb
    if snap.config_path.is_empty() {
        return "applied live — not saved (no config file; attach to a running build to persist)".into();
    }
    let overrides: BTreeMap<String, Value> = TUI_PERSIST
        .iter()
        .filter_map(|k| new.get(*k).map(|v| (k.to_string(), v.clone())))
        .collect();
    match crate::config::save_config_map(std::path::Path::new(&snap.config_path), &overrides) {
        Ok(_) => format!("saved {} settings → {}", overrides.len(), snap.config_path),
        Err(e) => format!("applied live; save failed: {}", e),
    }
}

/// Outcome of the TUI loop: the user quit, or (in first-run setup) chose ▶ Start build — which
/// returns the typed config the caller should launch the build with.
pub enum TuiResult {
    Quit,
    StartBuild(BTreeMap<String, Value>),
}

struct Ui {
    tab: usize,
    section: usize,          // the FOCUSED section within the tab (←/→ switches it)
    sel: usize,              // selected row WITHIN the focused section
    detail: bool,
    sel_key: Option<String>, // the SELECTED item's identity (stable selection: cursor follows it)
    detail_lines: Vec<String>, // detail content captured ONCE when the overlay opens (no per-frame I/O)
    dscroll: usize,          // scroll offset within the detail overlay
    setup: bool,             // first-run setup wizard (config locked, ▶ Start build / Cancel actions)
    cfg_fields: Vec<CfgField>, // the editable Configuration fields (built once from the config)
    cfg_active: usize,       // selected config field, or an action-button index past the fields
    fc_sel: usize,           // failures tab: the selected 'by cause' index — filters the families list
    cfg_flash: String,       // transient result of the last config 'apply' (shown on the config tab)
}

/// Number of sections on a tab (archive/config are single custom views → 1).
fn section_count(snap: &Snapshot, tab: usize) -> usize {
    match TABS[tab] {
        "config" | "archive" => 1,
        _ => sections_for(snap, tab, 0).len().max(1),
    }
}

/// The identity (slug/key) of each item in the FOCUSED section, in display order — used for stable
/// selection (the cursor tracks the item, not the row index, as lists reorder live).
fn list_keys(snap: &Snapshot, tab: usize, section: usize, fc_sel: usize) -> Vec<String> {
    match TABS[tab] {
        "config" => Vec::new(), // the config tab manages its own cursor (cfg_active), not ui.sel
        "archive" => snap.archive.pending.clone(),
        _ => sections_for(snap, tab, fc_sel).get(section).map(|s| s.keys.clone()).unwrap_or_default(),
    }
}

pub fn run(source: Arc<dyn Source>) -> std::io::Result<TuiResult> {
    run_mode(source, false)
}

/// Run the dashboard. `setup` = first-run wizard: lock to the config tab, disable Tab switching, and
/// offer ▶ Start build / Cancel (returns the typed config on Start).
pub fn run_mode(source: Arc<dyn Source>, setup: bool) -> std::io::Result<TuiResult> {
    let mut out = std::io::stdout();
    terminal::enable_raw_mode()?;
    queue!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    out.flush()?;

    // in setup the only view is config; a live monitor lands on overview
    let mut ui = Ui {
        tab: if setup { 0 } else { 1 },
        section: 0, sel: 0, detail: false, sel_key: None, detail_lines: Vec::new(), dscroll: 0,
        setup, cfg_fields: Vec::new(), cfg_active: 0, fc_sel: 0, cfg_flash: String::new(),
    };
    let mut prev: Option<Screen> = None;
    let res = loop {
        let snap = source.snapshot();
        // build the editable config fields ONCE, from the first snapshot that carries a config
        if ui.cfg_fields.is_empty() && !snap.config.is_empty() {
            ui.cfg_fields = cfg_init_fields(&snap.config);
        }
        // clamp the focused section to what this tab actually has (tabs differ in section count)
        let nsec = section_count(&snap, ui.tab);
        if ui.section >= nsec {
            ui.section = nsec.saturating_sub(1);
        }
        // stable selection: re-resolve the row index from the remembered item key each frame, so a
        // list reordering (failed→building→built) keeps the cursor on the SAME item.
        let keys = list_keys(&snap, ui.tab, ui.section, ui.fc_sel);
        if let Some(k) = &ui.sel_key {
            if let Some(i) = keys.iter().position(|x| x == k) {
                ui.sel = i;
            }
        }
        if ui.sel >= keys.len() {
            ui.sel = keys.len().saturating_sub(1);
        }
        // failures tab: when the 'Failures by cause' section is focused, its selection becomes the
        // sticky cause-filter for the families list (persists when you move to that list).
        if TABS[ui.tab] == "failures"
            && sections_for(&snap, ui.tab, ui.fc_sel).get(ui.section).map(|s| s.dview) == Some("failcat")
        {
            ui.fc_sel = ui.sel;
        }
        // draw the whole frame into a back-buffer, then emit only the cells that changed (no flicker)
        let (w, h) = terminal::size().unwrap_or((100, 40));
        let mut scr = Screen::new(w.max(1), h.max(1));
        render(&mut scr, &snap, &ui);
        flush_diff(&mut out, &scr, &prev)?;
        prev = Some(scr);

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != event::KeyEventKind::Press && k.kind != event::KeyEventKind::Repeat {
                    continue;
                }
                let on_config = TABS[ui.tab] == "config";
                // typing into a path/text field captures q / C / Space so they don't quit/reconfigure
                let text_active = on_config && !ui.detail && config_text_active(&ui);

                // --- global keys (Python order: Esc, q, C, Tab/BackTab) ---
                match k.code {
                    KeyCode::Esc => {
                        if ui.detail { ui.detail = false; } else { break TuiResult::Quit; }
                        continue;
                    }
                    KeyCode::Char('q') | KeyCode::Char('Q') if !text_active => break TuiResult::Quit,
                    KeyCode::Char('c') | KeyCode::Char('C') if !text_active && !ui.setup => {
                        if on_config {
                            // on the config tab, C applies+saves (same as the '✓ apply changes' action)
                            ui.cfg_flash = cfg_apply_live(&ui.cfg_fields, &snap, &*source);
                        } else {
                            // elsewhere, C jumps to the config tab (as the footer advertises)
                            ui.tab = TABS.iter().position(|&t| t == "config").unwrap_or(ui.tab);
                            ui.section = 0; ui.sel = 0; ui.sel_key = None; ui.detail = false;
                            ui.cfg_active = 0; ui.cfg_flash.clear();
                        }
                        continue;
                    }
                    KeyCode::Tab if !ui.setup => {
                        ui.tab = (ui.tab + 1) % TABS.len();
                        ui.section = 0; ui.sel = 0; ui.sel_key = None; ui.detail = false; ui.cfg_active = 0; ui.cfg_flash.clear();
                        continue;
                    }
                    KeyCode::BackTab if !ui.setup => {
                        ui.tab = (ui.tab + TABS.len() - 1) % TABS.len();
                        ui.section = 0; ui.sel = 0; ui.sel_key = None; ui.detail = false; ui.cfg_active = 0; ui.cfg_flash.clear();
                        continue;
                    }
                    _ => {}
                }

                // --- detail overlay (consumes navigation) ---
                if ui.detail {
                    match k.code {
                        KeyCode::Up => ui.dscroll = ui.dscroll.saturating_sub(1),
                        KeyCode::Down => ui.dscroll = (ui.dscroll + 1).min(ui.detail_lines.len().saturating_sub(1)),
                        // reset tab: the detail overlay IS the confirmation step — Enter to read,
                        // then D fires the deletion (refused daemon-side while builds are in flight)
                        KeyCode::Char('d') | KeyCode::Char('D') if TABS.get(ui.tab) == Some(&"reset") => {
                            if let Some(p) = snap.reset_portions.get(ui.sel) {
                                source.control(&ControlSet { reset_portion: Some(p.key.clone()), ..Default::default() });
                            }
                            ui.detail = false;
                        }
                        KeyCode::Enter | KeyCode::Backspace | KeyCode::Left => ui.detail = false,
                        _ => {}
                    }
                    continue;
                }

                // --- the unified Configuration editor ---
                if on_config {
                    if let Some(r) = handle_config_key(&mut ui, k.code, &snap, &*source) {
                        break r;
                    }
                    continue;
                }

                // --- other tabs: sections + items ---
                match k.code {
                    KeyCode::Up => {
                        if ui.sel > 0 { ui.sel -= 1; }
                        ui.sel_key = list_keys(&snap, ui.tab, ui.section, ui.fc_sel).get(ui.sel).cloned();
                    }
                    KeyCode::Down => {
                        ui.sel = (ui.sel + 1).min(list_keys(&snap, ui.tab, ui.section, ui.fc_sel).len().saturating_sub(1));
                        ui.sel_key = list_keys(&snap, ui.tab, ui.section, ui.fc_sel).get(ui.sel).cloned();
                    }
                    KeyCode::Left => {
                        let n = section_count(&snap, ui.tab);
                        if n > 1 { ui.section = (ui.section + n - 1) % n; ui.sel = 0; ui.sel_key = None; }
                    }
                    KeyCode::Right => {
                        let n = section_count(&snap, ui.tab);
                        if n > 1 { ui.section = (ui.section + 1) % n; ui.sel = 0; ui.sel_key = None; }
                    }
                    KeyCode::Enter => {
                        ui.detail_lines = build_detail(&snap, ui.tab, ui.section, ui.sel, ui.fc_sel, &source.build_dir());
                        if !ui.detail_lines.is_empty() { ui.dscroll = 0; ui.detail = true; }
                    }
                    // reset tab: D deletes the selected portion immediately (no overlay/confirm —
                    // matches the web button; outcome shows on the row as a bar then '✓ freed X')
                    KeyCode::Char('d') | KeyCode::Char('D') if TABS.get(ui.tab) == Some(&"reset") => {
                        if let Some(p) = snap.reset_portions.get(ui.sel) {
                            source.control(&ControlSet { reset_portion: Some(p.key.clone()), ..Default::default() });
                        }
                    }
                    KeyCode::Char('p') | KeyCode::Char('P') => {
                        source.control(&ControlSet { paused: Some(!snap.paused), ..Default::default() });
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        if let Some(slug) = selected_slug(&snap, ui.tab, ui.section, ui.sel, ui.fc_sel) {
                            source.control(&ControlSet { retry: Some(vec![slug]), ..Default::default() });
                        }
                    }
                    KeyCode::Char('o') | KeyCode::Char('O') => {
                        // re-queue every failed family we've written a gflib-build config override for
                        // (e.g. the whole instantiateUfo-bypass set) — rebuilds exactly the fixed ones
                        source.control(&ControlSet { retry_overrides: Some(true), ..Default::default() });
                    }
                    _ => {}
                }
            }
        }
    };

    queue!(out, terminal::LeaveAlternateScreen, cursor::Show, ResetColor)?;
    out.flush()?;
    terminal::disable_raw_mode()?;
    Ok(res)
}

/// True when the active config field is an editable path/text field — so q/C/Space type into it.
fn config_text_active(ui: &Ui) -> bool {
    let vis = cfg_visible(&ui.cfg_fields);
    if ui.cfg_active >= vis.len() {
        return false; // an action button
    }
    let f = &ui.cfg_fields[vis[ui.cfg_active]];
    cfg_editable(ui.setup, f) && matches!(f.kind, CfgKind::Path)
}

/// Handle a keypress on the Configuration tab. Returns Some(result) when the loop should end
/// (▶ Start build → StartBuild, Cancel → Quit). Mirrors the Python config-tab handler.
fn handle_config_key(ui: &mut Ui, code: KeyCode, snap: &Snapshot, src: &dyn Source) -> Option<TuiResult> {
    let vis = cfg_visible(&ui.cfg_fields);
    let actions = cfg_actions(ui.setup);
    let nav_n = vis.len() + actions.len();
    if nav_n == 0 {
        return None;
    }
    ui.cfg_active = ui.cfg_active.min(nav_n - 1);
    match code {
        KeyCode::Up => {
            ui.cfg_active = (ui.cfg_active + nav_n - 1) % nav_n;
            None
        }
        KeyCode::Down => {
            ui.cfg_active = (ui.cfg_active + 1) % nav_n;
            None
        }
        _ if ui.cfg_active >= vis.len() => {
            // an action button: Enter/Space activate it
            if matches!(code, KeyCode::Enter | KeyCode::Char(' ')) {
                let which = actions[ui.cfg_active - vis.len()];
                if which == "Cancel" {
                    return Some(TuiResult::Quit);
                }
                if ui.setup {
                    return Some(TuiResult::StartBuild(cfg_typed(&ui.cfg_fields))); // ▶ Start build
                }
                ui.cfg_flash = cfg_apply_live(&ui.cfg_fields, snap, src); // ✓ apply changes → control.json + persist
            }
            None
        }
        _ => {
            // edit the selected field (only if editable: setup, a live field, or a persistable setting)
            let fi = vis[ui.cfg_active];
            let editable = cfg_editable(ui.setup, &ui.cfg_fields[fi]);
            if editable && cfg_field_key(&mut ui.cfg_fields[fi], code) {
                ui.cfg_active = (ui.cfg_active + 1) % nav_n; // Enter advances to the next field
            }
            None
        }
    }
}

/// The slug to retry on [R] — only the focused section's selected family, and only for the sections
/// where a family identity is meaningful (failures / built / queue / history).
fn selected_slug(snap: &Snapshot, tab: usize, section: usize, sel: usize, fc_sel: usize) -> Option<String> {
    let secs = sections_for(snap, tab, fc_sel);
    let s = secs.get(section)?;
    if matches!(s.dview, "failures" | "built" | "queue" | "history") {
        s.keys.get(sel).cloned()
    } else {
        None
    }
}

// ---- flicker-free rendering: draw the whole frame into a back-buffer of cells, then emit ONLY the
// cells that changed since the previous frame (like ncurses does). No full-screen Clear → no flicker.
#[derive(Clone, PartialEq)]
struct Cell {
    ch: char,
    fg: Color,
    reverse: bool, // A_REVERSE equivalent (active tab, focused section header, selected row)
}
impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Reset, reverse: false }
    }
}
struct Screen {
    w: u16,
    h: u16,
    cells: Vec<Cell>,
}
impl Screen {
    fn new(w: u16, h: u16) -> Self {
        Screen { w, h, cells: vec![Cell::default(); (w as usize) * (h as usize)] }
    }
    fn set(&mut self, row: u16, col: u16, text: &str, fg: Color, reverse: bool) {
        if row >= self.h {
            return;
        }
        let mut c = col;
        for ch in text.chars().filter(|c| !c.is_control()) {
            if c >= self.w {
                break;
            }
            let idx = (row as usize) * (self.w as usize) + c as usize;
            self.cells[idx] = Cell { ch, fg, reverse };
            c += 1;
        }
    }
}

/// Draw `text` at (row,col) into the back-buffer `scr`. (`_w` kept for call-site compatibility.)
fn put(scr: &mut Screen, row: u16, col: u16, text: &str, color: Color, _w: u16) {
    scr.set(row, col, text, color, false);
}

/// Like `put`, but in reverse video (A_REVERSE) — for the active tab, focused section headers and
/// the selected row (matching the Python `curses.A_REVERSE` highlight, not a colour swap).
fn put_rev(scr: &mut Screen, row: u16, col: u16, text: &str, color: Color, reverse: bool) {
    scr.set(row, col, text, color, reverse);
}

/// Draw a sequence of coloured `(text, colour)` segments starting at (row,col) — a faithful port of
/// the Python multi-colour rows (e.g. cohort family names GREEN with CYAN " | " separators). When
/// `reverse` is set the whole row is drawn in reverse video as a single colour (the selected row).
fn put_segments(scr: &mut Screen, row: u16, col: u16, segs: &[(String, Color)], reverse: bool) {
    let mut c = col;
    if reverse {
        let joined: String = segs.iter().map(|(t, _)| t.as_str()).collect();
        scr.set(row, c, &joined, Color::White, true);
        return;
    }
    for (text, color) in segs {
        scr.set(row, c, text, *color, false);
        c += text.chars().count() as u16;
    }
}

/// Emit only the cells that differ from the previous frame (runs of same colour), so the terminal
/// never blanks. Full repaint when the size changed or there is no previous frame.
fn flush_diff(out: &mut Stdout, new: &Screen, prev: &Option<Screen>) -> std::io::Result<()> {
    let full = match prev {
        Some(p) => p.w != new.w || p.h != new.h,
        None => true,
    };
    for row in 0..new.h {
        let mut col = 0u16;
        while col < new.w {
            let idx = (row as usize) * (new.w as usize) + col as usize;
            let nc = &new.cells[idx];
            let changed = full || prev.as_ref().map(|p| p.cells.get(idx) != Some(nc)).unwrap_or(true);
            if !changed {
                col += 1;
                continue;
            }
            let fg = nc.fg;
            let rev = nc.reverse;
            let start = col;
            let mut run = String::new();
            while col < new.w {
                let i = (row as usize) * (new.w as usize) + col as usize;
                let cc = &new.cells[i];
                let ch_changed = full || prev.as_ref().map(|p| p.cells.get(i) != Some(cc)).unwrap_or(true);
                if !ch_changed || cc.fg != fg || cc.reverse != rev {
                    break;
                }
                run.push(cc.ch);
                col += 1;
            }
            queue!(out, cursor::MoveTo(start, row))?;
            if rev {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            queue!(out, SetForegroundColor(fg), Print(run), SetAttribute(Attribute::Reset), ResetColor)?;
        }
    }
    out.flush()
}

/// Humanize a phase id for the header, mirroring the Python `PHASE_LABEL` map.
fn phase_label(ph: &str) -> &str {
    match ph {
        "init" => "starting…",
        "clone_gf" => "cloning google/fonts",
        "build_fontc" => "building fontc from source",
        "toolchain" => "provisioning the pinned toolchain (fontc + builder3)",
        "discover" => "discovering worklist",
        "archive" => "populating archive (mirroring repos)",
        "cohorts" => "scanning dependency cohorts",
        "build" => "building",
        "done" => "done",
        _ => ph,
    }
}

// ---- section model: a tab is a stack of sections (Python's draw_sections). Each section's rows are
// materialized eagerly as coloured segments (like the Python fmt lambdas), with a per-row identity
// key (for stable selection + detail) and a detail-view tag (`dview`, "" = no detail). ----
struct SectionR {
    title: String,
    dview: &'static str,
    rows: Vec<Vec<(String, Color)>>,
    keys: Vec<String>,
}

fn task_mark(status: &str) -> &'static str {
    match status {
        "pending" => "⏳",
        "running" => "🔄",
        "done" => "✅",
        "failed" => "❌",
        "skipped" => "➖",
        _ => "?",
    }
}
fn task_color(status: &str) -> Color {
    match status {
        "done" => Color::Green,
        "failed" => Color::Red,
        "running" => Color::Yellow,
        "skipped" => Color::DarkGrey,
        _ => Color::Grey,
    }
}

/// Colour a fontc_crater verdict token. The fontc-can't-build cases (fontc-fail / both-fail /
/// src-miss) are magenta so they pop on our BUILT rows — that pairing is the gold (we compile what
/// fontc cannot). A fontmake-only failure is dim (fontc is fine there); a diff is yellow.
fn crater_color(tok: &str) -> Color {
    if tok.is_empty() {
        Color::DarkGrey
    } else if tok == "fontc-fail" || tok == "both-fail" || tok == "src-miss" {
        Color::Magenta
    } else if tok == "fmake-fail" {
        Color::DarkGrey
    } else if tok.starts_with('~') {
        Color::Yellow
    } else {
        Color::Cyan
    }
}

fn failures_section(snap: &Snapshot) -> SectionR {
    let rows = snap.failures_recent.iter().map(|f| {
        let mut segs = vec![(format!("{:<34} ", f.slug), Color::Red)];
        if !f.crater.is_empty() {
            // crater building this while we fail = a regression worth a closer look
            segs.push((format!("[cr:{:<10}] ", f.crater), crater_color(&f.crater)));
        }
        segs.push((f.error.clone(), Color::DarkRed));
        segs
    }).collect();
    SectionR {
        title: "Failures — newest first (current)".into(),
        dview: "failures",
        rows,
        keys: snap.failures_recent.iter().map(|f| f.slug.clone()).collect(),
    }
}

/// All sections of a tab, in display order — a port of the Python `sections_for`.
fn sections_for(snap: &Snapshot, tab: usize, fc_sel: usize) -> Vec<SectionR> {
    match TABS[tab] {
        "overview" => {
            let rows = snap.tasks.iter().map(|t| {
                let prog = if t.total > 0 { format!("{}/{}", t.done, t.total) } else { String::new() };
                let el = if t.elapsed > 0.0 { hms(t.elapsed) } else { String::new() };
                vec![(format!("{} {:<26} {:<11}{:>8}  {}", task_mark(&t.status), head(&t.name, 26), prog, el, t.detail), task_color(&t.status))]
            }).collect();
            vec![
                SectionR { title: "Pipeline".into(), dview: "overview", rows, keys: snap.tasks.iter().map(|t| t.key.clone()).collect() },
                { let mut s = failures_section(snap); s.title = "Recent failures".into(); s },
            ]
        }
        "queue" => {
            let kcol = |kind: &str| match kind { "retry" => Color::Yellow, "rebuild" => Color::Cyan, "upgrade" => Color::Magenta, _ => Color::Green };
            let rows = snap.queued_list.iter().map(|q| {
                let mut segs = vec![
                    (format!("  {:<8} ", q.kind), kcol(&q.kind)),
                    (format!("{:<38} ", head(&q.slug, 38)), Color::Grey),
                ];
                if !q.crater.is_empty() {
                    segs.push((format!("cr:{}", q.crater), crater_color(&q.crater)));
                }
                segs
            }).collect();
            vec![SectionR {
                title: "Queued — priority order (re-queued families first, then longest previous build first)".into(),
                dview: "queue", rows, keys: snap.queued_list.iter().map(|q| q.slug.clone()).collect(),
            }]
        }
        "cohorts" => {
            let rows = snap.cohorts.iter().map(cohort_segments).collect();
            vec![SectionR {
                title: "Dependency cohorts  (● = venv cached on disk)".into(),
                dview: "cohorts", rows, keys: snap.cohorts.iter().map(|c| c.key.clone()).collect(),
            }]
        }
        "built" => {
            let rows = snap.built_recent.iter().map(|b| {
                let comp = if !b.compiler_version.is_empty() { b.compiler_version.clone() } else { b.backend.clone() };
                let mut segs = vec![
                    (format!("{:<32} ", head(&b.slug, 32)), Color::Green),
                    (format!("{:<24} ", head(&comp, 24)), Color::Cyan),
                    (format!("{:>9}  {:<8}", human(b.bytes), b.compare), Color::Grey),
                ];
                if !b.crater.is_empty() {
                    // magenta token here = we built a family fontc_crater's fontc can't (the gold case)
                    segs.push((format!(" cr:{}", b.crater), crater_color(&b.crater)));
                }
                segs
            }).collect();
            vec![SectionR {
                title: "Built — successes  (slug · compiler+version · size · vs-shipped · crater)".into(),
                dview: "built", rows, keys: snap.built_recent.iter().map(|b| b.slug.clone()).collect(),
            }]
        }
        "packaging" => {
            // section 1: the deb-build external toolchain (auto-detected; recovers within ~5s).
            let dt_rows = snap.deb_tools.iter().map(|t| {
                if t.present {
                    vec![
                        ("✓ ".to_string(), Color::Green),
                        (format!("{:<20} ", t.name), Color::White),
                        (t.purpose.clone(), Color::Grey),
                    ]
                } else {
                    vec![
                        ("✗ ".to_string(), Color::Red),
                        (format!("{:<20} ", t.name), Color::White),
                        (format!("MISSING — sudo apt install {}", t.provides), Color::Yellow),
                    ]
                }
            }).collect();
            // section 2: per-family packaging status (reuses built_recent + the packaged flag;
            // dview "built" so ENTER opens the existing built detail overlay unchanged).
            let rows = snap.packages.iter().map(|b| {
                let (status, scol) = match b.deb_status.as_str() {
                    "lint-clean" => ("✓ lint-clean", Color::Green), // validated + lintian clean (strictest)
                    "lint-warn" => ("lint-warn", Color::Yellow),    // validated + lintian warnings, no errors
                    "lintian-fail" => ("lintian-fail", Color::Red), // validated by dpkg-deb but lintian errors
                    "validated" => ("validated", Color::DarkCyan),  // dpkg-deb ok; lintian not run yet
                    "built" => ("built", Color::Cyan),
                    "failed" => ("deb-failed", Color::DarkRed),
                    _ if b.packaged => ("drafted", Color::DarkYellow),
                    _ => ("draftable", Color::Grey),
                };
                let comp = if !b.compiler_version.is_empty() { b.compiler_version.clone() } else { b.backend.clone() };
                vec![
                    (format!("{:<12} ", status), scol),
                    (format!("{:<32} ", head(&b.slug, 32)), Color::Grey),
                    (format!("{:<26} ", head(&comp, 26)), Color::Cyan),
                    (format!("{:>9}", human(b.bytes)), Color::Grey),
                ]
            }).collect();
            // packaging/lint queue summary (mirrors the main build queue: live activity + backlog)
            let lint_avail = snap.deb_tools.iter().any(|t| t.name == "lintian" && t.present);
            let (act, acol) = if !snap.pkg_now.is_empty() {
                (format!("▶ {}", snap.pkg_now), Color::Yellow)
            } else if snap.paused {
                ("paused (the global pause also halts packaging/linting)".into(), Color::Grey)
            } else {
                ("idle — nothing to package or lint".into(), Color::Grey)
            };
            // stage 1: packaging (build the .deb); stage 2: lintian — one line each, packaging above lint
            let built = snap.counts.built;
            let packaged = built.saturating_sub(snap.pkg_pending);
            let ppct = if built > 0 { 100 * packaged / built } else { 0 };
            let lpct = if snap.lint_total > 0 { 100 * snap.lint_done / snap.lint_total } else { 0 };
            let pkg_row = format!("packaging  {}/{} ({}%)   ·   to package: {}", packaged, built, ppct, snap.pkg_pending);
            let lint_row = format!(
                "lintian    {}/{} ({}%)   ·   to lint: {}{}",
                snap.lint_done, snap.lint_total, lpct, snap.lint_pending,
                if lint_avail { "" } else { "   ·   lintian NOT installed — lint queue stalled" }
            );
            let q_rows: Vec<Vec<(String, Color)>> = vec![
                vec![(format!("  {}", act), acol)],
                vec![(format!("  {}", pkg_row), Color::Cyan)],
                vec![(format!("  {}", lint_row), if lint_avail { Color::Green } else { Color::Red })],
            ];
            let mut secs = vec![
                SectionR { title: "Packaging queue".into(), dview: "", rows: q_rows, keys: vec![] },
                SectionR {
                    title: "Deb toolchain  (install any ✗ to enable deb building/validation — auto-detected, recovers in ~5s)".into(),
                    dview: "", rows: dt_rows, keys: snap.deb_tools.iter().map(|t| t.name.clone()).collect(),
                },
            ];
            if !snap.lint_categories.is_empty() {
                let lc_rows: Vec<Vec<(String, Color)>> = snap.lint_categories.iter().map(|c| {
                    let col = if c.severity == "E" { Color::Red } else { Color::DarkYellow };
                    vec![
                        (format!("{:>4}  ", c.count), col),
                        (format!("{} {:<42}", if c.severity == "E" { "E" } else { "W" }, head(&c.tag, 42)), col),
                        (format!(" {} pkgs", c.count), Color::Grey),
                    ]
                }).collect();
                secs.push(SectionR {
                    title: "Lintian findings by category  (E = error · W = warning · ENTER = affected packages)".into(),
                    dview: "lintcat", rows: lc_rows,
                    keys: snap.lint_categories.iter().map(|c| format!("{}:{}", c.severity, c.tag)).collect(),
                });
            }
            secs.push(SectionR {
                title: "Packaging — per-family status  (draftable → drafted → built → validated → lintian: clean / warn / fail · ENTER = debian/ metadata)".into(),
                dview: "package", rows, keys: snap.packages.iter().map(|b| b.slug.clone()).collect(),
            });
            secs
        }
        "reset" => {
            // granular system reset: one row per deletable portion, with its live on-disk size.
            // ENTER explains what a deletion does; D inside the overlay confirms it.
            let rows = snap.reset_portions.iter().map(|p| {
                if p.deleting {
                    // live deletion: textual progress bar + the remaining bytes counting down
                    let tot = p.bytes.max(1);
                    let fr = p.freed.min(tot);
                    let pct = (fr * 100 / tot) as usize;
                    let fill = pct * 24 / 100;
                    let bar: String = (0..24).map(|i| if i < fill { '#' } else { '-' }).collect();
                    return vec![
                        (format!("{:<44} ", head(&p.label, 44)), Color::White),
                        (format!("[{}] {:>3}%  ", bar, pct), Color::Red),
                        (format!("{} remaining", crate::util::human(tot - fr)), Color::Yellow),
                    ];
                }
                let tail = if !p.note.is_empty() {
                    let col = if p.note.starts_with('⛔') { Color::Red } else { Color::Green };
                    (p.note.clone(), col)
                } else {
                    (head(&p.hint, 70), Color::Grey)
                };
                vec![
                    (format!("{:<44} ", head(&p.label, 44)), Color::White),
                    (format!("{:>10}  ", crate::util::human(p.bytes)), if p.bytes > 0 { Color::Yellow } else { Color::DarkGrey }),
                    tail,
                ]
            }).collect();
            vec![SectionR {
                title: "Reset — delete a portion of the build system  (D = delete the selected row · ENTER = details · items in use by a running build are kept · archive & results are never touched)".into(),
                dview: "reset", rows, keys: snap.reset_portions.iter().map(|p| p.key.clone()).collect(),
            }]
        }
        "tools" => {
            // build-tool packages, classified python/rust: the M5 (Python->Rust) burn-down view.
            // python = a remaining migration blocker; rust = already a native tool.
            let rows = snap.tool_packages.iter().map(|t| {
                let lcol = if t.lang == "rust" { Color::Green } else { Color::Yellow };
                let status = if t.packaged { "packaged" } else { "unpackaged" };
                vec![
                    (format!("{:<7} ", t.lang), lcol),
                    (format!("{:<24} ", head(&t.name, 24)), Color::White),
                    (format!("{:<12} ", t.kind), Color::Cyan),
                    (format!("{:>4} families  ", t.families), Color::Grey),
                    (status.to_string(), Color::DarkGrey),
                ]
            }).collect();
            let ms_rows = MILESTONES.iter().map(|(m, d)| vec![
                (format!("{:<4}", m), Color::Cyan),
                (d.to_string(), Color::Grey),
            ]).collect();
            vec![
                SectionR {
                    title: "Build-tool packages  (python = M5 blocker · rust = native · ENTER = which families need it)".into(),
                    dview: "tool", rows, keys: snap.tool_packages.iter().map(|t| t.name.clone()).collect(),
                },
                SectionR {
                    title: "Migration milestones (M0–M7) — what the rungs mean".into(),
                    dview: "", rows: ms_rows, keys: Vec::new(),
                },
            ]
        }
        "failures" => {
            let mut secs = Vec::new();
            let has_cats = !snap.fail_categories.is_empty();
            // the selected cause (sticky); its families filter the 'newest' list below
            let sel_cat = if has_cats { snap.fail_categories.get(fc_sel.min(snap.fail_categories.len() - 1)) } else { None };
            if has_cats {
                let sel_idx = fc_sel.min(snap.fail_categories.len().saturating_sub(1));
                let rows = snap.fail_categories.iter().enumerate().map(|(i, c)| {
                    let on = i == sel_idx;
                    vec![
                        (format!("{}{:>4}  ", if on { "▸" } else { " " }, c.count), Color::White),
                        (format!("{:<24}", head(&c.cat, 24)), if on { Color::Yellow } else { Color::Cyan }),
                        (format!(" {}", c.hint), Color::DarkGrey),
                    ]
                }).collect();
                secs.push(SectionR {
                    title: "Failures by cause  (←/→ here, ↑/↓ to pick — filters the list below)".into(), dview: "failcat", rows,
                    keys: snap.fail_categories.iter().map(|c| c.cat.clone()).collect(),
                });
            }
            // families list, scoped to the selected cause. We list the cause's OWN
            // families (the authoritative full set behind the count) rather than
            // intersecting with the capped 'recent' window — otherwise a cause whose
            // families fell out of the recent window shows "(none)" despite a non-zero
            // count. Errors are looked up from recent first, then failure-history.
            let (frows, fkeys, ftitle): (Vec<Vec<(String, Color)>>, Vec<String>, String) = match sel_cat {
                Some(c) => {
                    let err_for = |slug: &str| -> Option<String> {
                        snap.failures_recent.iter().find(|f| f.slug == slug).map(|f| f.error.clone())
                            .or_else(|| snap.failure_history.iter().rev().find(|h| h.slug == slug).map(|h| h.error.clone()))
                    };
                    let rows = c.families.iter().map(|slug| {
                        let (etxt, ecol) = match err_for(slug) {
                            Some(e) if !e.is_empty() => (e, Color::DarkRed),
                            _ => ("(no recorded error text)".to_string(), Color::DarkGrey),
                        };
                        vec![(format!("{:<34} ", slug), Color::Red), (etxt, ecol)]
                    }).collect();
                    (rows, c.families.clone(), format!("Families failed — cause: {}  ({})", c.cat, c.count))
                }
                None => {
                    let rows = snap.failures_recent.iter().map(|f| vec![
                        (format!("{:<34} ", f.slug), Color::Red),
                        (f.error.clone(), Color::DarkRed),
                    ]).collect();
                    (rows, snap.failures_recent.iter().map(|f| f.slug.clone()).collect(),
                     "Failures — newest first (current)".into())
                }
            };
            secs.push(SectionR {
                title: ftitle, dview: "failures", rows: frows, keys: fkeys,
            });
            if !snap.failure_history.is_empty() {
                let rows = snap.failure_history.iter().map(|h| vec![
                    (format!("{:<20} ", head(&h.cause, 20)), Color::Yellow),
                    (h.slug.clone(), Color::Grey),
                ]).collect();
                secs.push(SectionR {
                    title: "Failure history (persistent — survives restarts & re-attempts)".into(),
                    dview: "history", rows, keys: snap.failure_history.iter().map(|h| h.slug.clone()).collect(),
                });
            }
            secs
        }
        "stats" => {
            let mut phases: Vec<(String, f64)> = snap.phase_durations.iter().map(|(k, v)| (k.clone(), *v)).collect();
            phases.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let prows = phases.iter().map(|(k, v)| vec![(format!("{:<12} {}", k, hms(*v)), Color::Grey)]).collect();
            let mut ops: Vec<(String, crate::model::OpStat)> = snap.op_stats.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            ops.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap_or(std::cmp::Ordering::Equal));
            let orows = ops.iter().map(|(k, s)| vec![(format!("{:<10} total {:>9.1}  n {:>5}  mean {:>7.2}  max {:>7.1}", k, s.total, s.count, s.mean, s.max), Color::Cyan)]).collect();
            let mut sections = vec![
                SectionR { title: "Phase timing".into(), dview: "", rows: prows, keys: phases.iter().map(|(k, _)| k.clone()).collect() },
                SectionR { title: "Operation timing".into(), dview: "stats", rows: orows, keys: ops.iter().map(|(k, _)| k.clone()).collect() },
            ];
            if !snap.python_versions.is_empty() {
                let pyrows = snap.python_versions.iter()
                    .map(|(v, n)| vec![(format!("Python {:<10} {:>6} families", v, n), Color::Green)]).collect();
                sections.push(SectionR { title: "Python interpreters (multi-Python ladder)".into(), dview: "",
                    rows: pyrows, keys: snap.python_versions.keys().cloned().collect() });
            }
            sections
        }
        "fontspector" => match &snap.fontspector {
            None => vec![SectionR {
                title: "fontspector QA — no results yet (run: gflib-build --fontspector)".into(),
                dview: "", rows: Vec::new(), keys: Vec::new(),
            }],
            Some(fs) => {
                // panel B: a check across all families (most failures first)
                let crows = fs.per_check.iter().map(|c| {
                    let f = c.counts.fail + c.counts.fatal + c.counts.error;
                    let sc = if f > 0 { Color::Red } else if c.counts.warn > 0 { Color::Yellow } else { Color::Green };
                    vec![
                        (format!("{:>4} fail {:>4} warn  ", f, c.counts.warn), sc),
                        (head(&c.id, 48), Color::White),
                    ]
                }).collect();
                // panel A: per family (worst status first)
                let frows = fs.per_family.iter().map(|f| {
                    let fc = f.counts.fail + f.counts.fatal + f.counts.error;
                    vec![
                        (format!("{:<6} ", f.worst), fs_color(&f.worst)),
                        (format!("{:<42} ", head(&f.slug, 42)), Color::Grey),
                        (format!("{} fail · {} warn · {} pass", fc, f.counts.warn, f.counts.pass), Color::DarkGrey),
                    ]
                }).collect();
                vec![
                    SectionR {
                        title: format!("Checks — most failures first  [{} · profile {} · {} families]", fs.version, fs.profile, fs.families_checked),
                        dview: "fscheck", rows: crows, keys: fs.per_check.iter().map(|c| c.id.clone()).collect(),
                    },
                    SectionR {
                        title: "Families — worst status first".into(),
                        dview: "fsfamily", rows: frows, keys: fs.per_family.iter().map(|f| f.slug.clone()).collect(),
                    },
                ]
            }
        },
        "crater" => match &snap.crater {
            None => vec![SectionR {
                title: "fontc_crater — not loaded  (put fontc_crater_targets.json in gflib-data, or run gfonts_agents' fetch_crater_analysis.py; --no-crater disables)".into(),
                dview: "", rows: Vec::new(), keys: Vec::new(),
            }],
            Some(cv) => {
                let partial = if cv.complete { String::new() } else {
                    "   [PARTIAL: diff-only fallback — run fetch_crater_analysis.py for the fontc/both-failed split]".into()
                };
                let sum = vec![
                    vec![(format!("crater run {} · fontc {} · google/fonts {}{}",
                        cv.run, head(&cv.fontc_rev, 12), head(&cv.fonts_repo_sha, 12), partial), Color::Grey)],
                    vec![(format!("matched {} of our families to a crater verdict", cv.matched), Color::White)],
                    vec![(format!("  GOLD  we build · fontc can't : {:>4}   (upstream-worthy build fixes)", cv.we_build_fontc_cant), Color::Magenta)],
                    vec![(format!("  REGR  we fail  · fontc built : {:>4}   (our build bugs)", cv.we_fail_fontc_ok), Color::Red)],
                    vec![(format!("  both build · identical       : {:>4}", cv.both_ok_identical), Color::Green)],
                    vec![(format!("  both build · output differs  : {:>4}", cv.both_ok_diff), Color::Yellow)],
                    vec![(format!("crater verdicts (matched): match {} · diff {} · fontc-fail {} · fmake-fail {} · both-fail {} · src-miss {}",
                        cv.c_identical, cv.c_diff, cv.c_fontc_failed, cv.c_fontmake_failed, cv.c_both_failed, cv.c_repo_failed), Color::DarkGrey)],
                ];
                let grows = cv.gold_families.iter().map(|s| vec![
                    (format!("{:<50} ", head(s, 50)), Color::Magenta),
                    ("fontc can't build this — our build fix is upstream-worthy".into(), Color::DarkGrey),
                ]).collect();
                let rrows = cv.regression_families.iter().map(|s| vec![
                    (format!("{:<50} ", head(s, 50)), Color::Red),
                    ("fontc builds this but we don't — likely our bug".into(), Color::DarkGrey),
                ]).collect();
                vec![
                    SectionR { title: "fontc_crater comparison — summary".into(), dview: "", rows: sum, keys: Vec::new() },
                    SectionR {
                        title: format!("GOLD — we build, fontc_crater's fontc cannot ({})   ·   refresh with:  --retrigger-crater fontc-failed", cv.we_build_fontc_cant),
                        dview: "built", rows: grows, keys: cv.gold_families.clone(),
                    },
                    SectionR {
                        title: format!("Regressions — fontc builds it, we fail ({})", cv.we_fail_fontc_ok),
                        dview: "failures", rows: rrows, keys: cv.regression_families.clone(),
                    },
                ]
            }
        },
        _ => Vec::new(),
    }
}

/// Stack a tab's sections into `avail` rows: the focused section header is reverse-video, its
/// selected row reversed, others a peek. Row budget is shared via water-fill so a small section
/// shows whole while the focused/large one expands — the visual equivalent of Python's draw_sections.
fn draw_sections(scr: &mut Screen, secs: &[SectionR], top: u16, avail: u16, w: u16, focus: usize, sel: usize) {
    if secs.is_empty() || avail == 0 {
        return;
    }
    let desired: Vec<usize> = secs.iter().map(|s| 1 + s.rows.len().max(1)).collect();
    let alloc = water_fill(&desired, avail as usize);
    let bottom = top + avail;
    let mut row = top;
    for (si, sec) in secs.iter().enumerate() {
        if row >= bottom {
            break;
        }
        let foc = si == focus;
        let mut hdr = format!(" {}[{}] {} ", if foc { "▼ " } else { "▷ " }, sec.rows.len(), sec.title);
        while hdr.chars().count() < (w as usize).saturating_sub(1) {
            hdr.push('-');
        }
        put_rev(scr, row, 0, &hdr, if foc { Color::White } else { Color::DarkGrey }, foc);
        row += 1;
        let body_rows = alloc[si].saturating_sub(1);
        if sec.rows.is_empty() {
            if row < bottom {
                put(scr, row, 1, "(none)", Color::DarkGrey, w);
                row += 1;
            }
            continue;
        }
        let start = if foc { scroll_start(sel, body_rows, sec.rows.len()) } else { 0 };
        for (i, segs) in sec.rows.iter().enumerate().skip(start).take(body_rows) {
            if row >= bottom {
                break;
            }
            put_segments(scr, row, 1, segs, foc && i == sel);
            row += 1;
        }
        if sec.rows.len() > start + body_rows && row < bottom {
            put(scr, row, 1, &format!("  … (+{} more)", sec.rows.len() - start - body_rows), Color::DarkGrey, w);
            row += 1;
        }
    }
}

fn render(scr: &mut Screen, snap: &Snapshot, ui: &Ui) {
    let (w, h) = (scr.w, scr.h);

    // ---- header (row 0/1) — matches Python exactly: title + [PAUSED], elapsed at w-24 ----
    let pause_tag = if !snap.paused {
        String::new()
    } else if snap.running_builds > 0 {
        format!(" [PAUSED · {} {} frozen]", snap.running_builds, if snap.running_builds == 1 { "build" } else { "builds" })
    } else {
        " [PAUSED]".to_string()
    };
    let title = format!(" Google Fonts library build{}", pause_tag);
    put(scr, 0, 0, &title, Color::White, w);

    // first-run setup: no disk/progress rows — just a one-line instruction (the config tab is below)
    let pre_build = ui.setup || snap.pre_build;
    if pre_build {
        put(scr, 0, w.saturating_sub(18), "first-time setup", Color::DarkGrey, w);
        put(scr, 1, 0, " configure your build below, then navigate to ▶ Start build", Color::Cyan, w);
    } else {
        put(scr, 0, w.saturating_sub(24), &format!("elapsed {}", hms(snap.elapsed)), Color::White, w);
        let bld = snap.disk_build_total;
        let arc = snap.disk_archive_total;
        // Always spell out both components — no ambiguous "(build dir)".
        let disk = if snap.disk_archive_nested {
            format!("disk used {} (build + nested archive, all included)", human(bld))
        } else {
            format!("disk used {} (build {} + archive {})", human(bld + arc), human(bld), human(arc))
        };
        // when builds are frozen by a lowered job limit (not a global pause), note it next to jobs
        let job_frozen = if !snap.paused && snap.frozen_builds > 0 {
            format!(" ({} frozen→draining)", snap.frozen_builds)
        } else {
            String::new()
        };
        put(
            scr,
            1,
            0,
            &format!(
                " {}  free {}  jobs {}{}  cohorts {}  both {}/fontc {}/fontmake {}",
                disk, human(snap.disk_free), snap.jobs, job_frozen, snap.cohorts.len(),
                snap.backends.both, snap.backends.fontc, snap.backends.fontmake
            ),
            Color::Cyan,
            w,
        );
        render_progress(scr, snap, w);
    }

    // ---- tab bar (row 4) — active tab in reverse video, inactive dim, + switch hint ----
    render_tabbar_body(scr, snap, ui, w, h);
}

/// The phase line + segmented progress bar (rows 2/3). Extracted so the setup wizard can omit it.
fn render_progress(scr: &mut Screen, snap: &Snapshot, w: u16) {
    // The bar measures progress over the IN-SCOPE worklist (built + failed + queued + building =
    // total − skipped). 'skipped' = NOT selected this run (outside the % sample / --only); counting
    // it as done made the bar read ~100% even when most of the library was never attempted.
    let c = &snap.counts;
    let inscope = c.built + c.failed + c.queued + c.building;
    let done = c.built + c.failed;
    let ph = snap.phase.as_str();
    let plabel = phase_label(ph);
    let segmented;
    let frac;
    let bar_label;
    if (ph == "archive" || ph == "cohorts") && snap.phase_total > 0 {
        let (pd, pt) = (snap.phase_done, snap.phase_total);
        put(scr, 2, 0, &format!(" Phase: {}  {}/{}  {}", plabel, pd, pt, trunc(&snap.phase_label, 30)), Color::Yellow, w);
        frac = pd as f64 / pt.max(1) as f64;
        segmented = false;
        bar_label = format!(" {}/{} {} ({}%) ", pd, pt, plabel, (100.0 * frac) as u64);
    } else {
        put(scr, 2, 0, &format!(" Phase: {}   built {}  failed {}  building {}  queued {}",
            plabel, c.built, c.failed, c.building, c.queued), Color::White, w);
        // skipped = NOT selected this run — surface it with the fix (raise % / drop --only)
        if c.skipped > 0 {
            let hint = format!("{} skipped (not selected — raise % to 100 to build them)", c.skipped);
            put(scr, 2, w.saturating_sub(hint.chars().count() as u16 + 1), &hint, Color::Yellow, w);
        }
        frac = done as f64 / inscope.max(1) as f64;
        segmented = true;
        bar_label = format!(" {}/{} attempted ({}%){} ", done, inscope, (100.0 * frac) as u64,
            if c.skipped > 0 { format!(" · {} skipped", c.skipped) } else { String::new() });
    }
    if !snap.phase_error.is_empty() {
        put(scr, 2, w.saturating_sub(30), &format!("ERR {}", trunc(&snap.phase_error, 24)), Color::Red, w);
    }
    let barw = (w.saturating_sub(4)).max(10) as usize;
    if segmented {
        // colour the IN-SCOPE bar by outcome: built (green) · failed (red) · not-yet-attempted (dim).
        // the built (green) span is split by compiler in three shades, ordered as a pipeline
        // toward the end-goal: both · fontc · fontmake (brightest→darkest), matching the web bar.
        let base = inscope.max(1);
        let bw = barw * c.built / base;
        let fw = barw * c.failed / base;
        let rest = barw.saturating_sub(bw + fw); // queued + building (not yet attempted)
        // sub-widths of the built span, in the SAME order as the web (both, fontc, fontmake),
        // with a base-green remainder so the three always sum to bw
        let wfc = barw * snap.backends.fontc / base;
        let wfm = barw * snap.backends.fontmake / base;
        let wboth = barw * snap.backends.both / base;
        let wother = bw.saturating_sub(wfc + wfm + wboth);
        const FONTC: Color = Color::Rgb { r: 34, g: 197, b: 94 };   // medium green
        const FONTMAKE: Color = Color::Rgb { r: 21, g: 128, b: 61 }; // darker green
        const BOTH: Color = Color::Rgb { r: 74, g: 222, b: 128 };    // lightest/brightest green
        put(scr, 3, 1, "[", Color::White, w);
        let mut x = 2u16;
        for (width, col) in [(wboth, BOTH), (wfc, FONTC), (wfm, FONTMAKE), (wother, Color::Green)] {
            if width > 0 { put(scr, 3, x, &"#".repeat(width), col, w); x += width as u16; }
        }
        if fw > 0 { put(scr, 3, x, &"#".repeat(fw), Color::Red, w); x += fw as u16; }
        if rest > 0 { put(scr, 3, x, &"-".repeat(rest), Color::DarkGrey, w); x += rest as u16; }
        put(scr, 3, x, "]", Color::White, w);
    } else {
        let fill = (barw as f64 * frac) as usize;
        put(scr, 3, 1, &format!("[{}{}]", "#".repeat(fill), "-".repeat(barw.saturating_sub(fill))), Color::Cyan, w);
    }
    // bold centred label overlaid on the bar
    put(scr, 3, (barw as u16 / 2).max(2), &bar_label, Color::White, w);
}

/// The tab bar (row 4) + pinned now-building + per-tab body + status panel + footer + detail overlay.
fn render_tabbar_body(scr: &mut Screen, snap: &Snapshot, ui: &Ui, w: u16, h: u16) {
    // ---- tab bar (row 4) — active tab in reverse video, inactive dim, + switch hint ----
    let mut x = 1u16;
    for name in TABS.iter() {
        let label = format!(" {} ", name);
        let active = *name == TABS[ui.tab];
        put_rev(scr, 4, x, &label, if active { Color::White } else { Color::DarkGrey }, active);
        x += label.chars().count() as u16;
    }
    put(scr, 4, x.saturating_add(2).max(w.saturating_sub(24)), "[Tab]/[⇧Tab] switch tabs", Color::DarkGrey, w);

    // ---- always-on status panel: compute the focus info first, since it reserves body rows ----
    // (matches Python: panel_h = 1 separator + N info lines, body renders above it.)
    let info = focus_info(snap, ui);
    let panel_h = if info.is_empty() { 0 } else { 1 + info.len() as u16 };
    let footer_row = h.saturating_sub(1);
    let sep_row = footer_row.saturating_sub(panel_h);

    // ---- pinned "Now building": shown on EVERY tab (incl. overview) while families compile and no
    // detail overlay is open — a faithful port of the Python pinned block (yellow, capped, overflow).
    let body_top = 6u16;
    let mut row = body_top;
    if !snap.building.is_empty() && !ui.detail && sep_row.saturating_sub(row) >= 3 {
        // Break the count down by STAGE: only the compile stage (run_builder) can be paused/frozen; the
        // venv-install ("installing deps") + checkout stages can't, which is why pausing/lowering jobs
        // doesn't visibly freeze families still installing dependencies.
        let total = snap.building.len();
        let compile = snap.running_builds.min(total);
        let frozen = snap.frozen_builds.min(compile);
        let other = total.saturating_sub(compile); // venv-install / checkout / pre-build (not freezable)
        let mut parts: Vec<String> = Vec::new();
        if compile > frozen { parts.push(format!("{} compiling", compile - frozen)); }
        if frozen > 0 { parts.push(format!("{} frozen", frozen)); }
        if other > 0 { parts.push(format!("{} installing/setup", other)); }
        let mut hdr = if parts.len() > 1 {
            format!(" ▶ Now building ({} — {}) ", total, parts.join(", "))
        } else {
            format!(" ▶ Now building ({}) ", total)
        };
        while hdr.chars().count() < (w as usize).saturating_sub(1) {
            hdr.push('-');
        }
        put(scr, row, 0, &hdr, Color::Yellow, w);
        row += 1;
        // Show ALL in-flight families in COLUMNS (no '+N more' cap) so the pinned block stays compact.
        // State is by COLOUR + a short tag: blue FRZ = SIGSTOP-frozen compile; magenta INS→ = an install
        // over the limit, draining toward a freeze; yellow = actively building. (Per-row detail in the web UI.)
        let over_limit = snap.paused || total.saturating_sub(snap.frozen_builds) > snap.jobs;
        let colw: usize = 34;
        let cols = ((w as usize) / colw).max(1);
        let avail = sep_row.saturating_sub(row) as usize; // rows before the status panel
        let rows_needed = total.div_ceil(cols);
        let rows_used = rows_needed.min(avail.saturating_sub(1).max(1)); // keep a trailing blank
        let shown = (rows_used * cols).min(total);
        for (i, b) in snap.building.iter().take(shown).enumerate() {
            let cr = row + (i / cols) as u16;
            let cx = ((i % cols) * colw) as u16;
            let pending = !b.frozen && b.note == "installing deps" && over_limit;
            let (color, tag) = if b.frozen {
                (Color::Blue, "FRZ ")
            } else if pending {
                (Color::Magenta, "INS\u{2192}") // installing — will freeze on reaching compile
            } else {
                (Color::Yellow, "    ")
            };
            let slugw = colw.saturating_sub(12); // tag(4) + slug + ' ' + dur(6) + gap
            put(scr, cr, cx + 1, &format!("{}{:<sw$} {:>6}", tag, head(&b.slug, slugw), hms(b.dur), sw = slugw), color, w);
        }
        row += rows_used as u16;
        if shown < total {
            put(scr, row, 1, &format!("  … (+{} more — enlarge the terminal to see all)", total - shown), Color::DarkGrey, w);
            row += 1;
        }
        row += 1;
    }

    // ---- body per tab ----
    let avail = sep_row.saturating_sub(row);
    match TABS[ui.tab] {
        "archive" => render_archive(scr, snap, row, avail, w),
        "config" => render_config(scr, snap, ui, row, sep_row, w),
        "stats" => {
            // Python's stats view = a fontc-migration summary, then the timing sections below it
            let used = render_stats_prefix(scr, snap, row, w);
            let r2 = row + used;
            draw_sections(scr, &sections_for(snap, ui.tab, ui.fc_sel), r2, sep_row.saturating_sub(r2), w, ui.section, ui.sel);
        }
        _ => draw_sections(scr, &sections_for(snap, ui.tab, ui.fc_sel), row, avail, w, ui.section, ui.sel),
    }

    // ---- status panel (separator + up to 3 context lines) ----
    if !info.is_empty() {
        put(scr, sep_row, 0, &"─".repeat(w as usize), Color::DarkGrey, w);
        for (i, ln) in info.iter().enumerate() {
            put(scr, sep_row + 1 + i as u16, 0, ln, Color::White, w);
        }
    }

    // ---- footer (mode-dependent, matching the Python help strings) ----
    let footer = if ui.detail {
        " [esc/←/↵] back to list   [↑↓] scroll"
    } else if ui.setup {
        " [↑↓] field   [←→/space] edit   navigate to ▶ Start build and press ↵   [esc] cancel"
    } else if TABS[ui.tab] == "config" {
        " [↑↓]field  [←→/space]edit  [↵ or C]apply+save  [Tab/⇧Tab]tabs  [q]uit"
    } else {
        " [Tab/⇧Tab]tabs  [↑↓]item  [←→]section  [↵]details  [p]ause  [R]etry  [O]retry-config-fixed  [C]onfig  [q]uit"
    };
    put(scr, footer_row, 0, footer, Color::DarkGrey, w);
    if TABS[ui.tab] == "config" && !ui.cfg_flash.is_empty() {
        put(scr, footer_row.saturating_sub(1), 0, &format!(" ⓘ {}", ui.cfg_flash), Color::Green, w);
    }

    // detail overlay (drawn on top of the back-buffer)
    if ui.detail {
        render_detail(scr, ui, w, h);
    }
}

/// Share `avail` rows fairly across sections wanting `desired[i]` rows each: a section that needs
/// fewer than its fair share releases the surplus to the others (water-fill), so a small section
/// shows in full while a large one expands to take the rest. Converges in a few rounds.
fn water_fill(desired: &[usize], avail: usize) -> Vec<usize> {
    let n = desired.len();
    let mut alloc = vec![0usize; n];
    let mut remaining = avail;
    loop {
        let unsat: Vec<usize> = (0..n).filter(|&i| alloc[i] < desired[i]).collect();
        if unsat.is_empty() || remaining == 0 {
            break;
        }
        let share = (remaining / unsat.len()).max(1);
        let mut used = 0;
        for &i in &unsat {
            if used >= remaining {
                break;
            }
            let give = (desired[i] - alloc[i]).min(share).min(remaining - used);
            alloc[i] += give;
            used += give;
        }
        if used == 0 {
            break;
        }
        remaining -= used;
    }
    alloc
}

/// Python-style `s[:n]` slice (no ellipsis), used by the fixed-width row columns.
fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Colour for a cohort member by build status: built=green, failed=red, building=yellow,
/// not-yet-attempted (queued/pending/…)=grey. Shared by the TUI cohort row and the detail overlay.
fn fam_color(status: &str) -> Color {
    match status {
        "built" => Color::Green,
        "failed" => Color::Red,
        "building" => Color::Yellow,
        _ => Color::DarkGrey,
    }
}

/// The cohort row as coloured segments: a cached/uncached dot, `count + key` in the cohort colour
/// (CYAN, or default for "base"), then the family NAMES coloured by build status with CYAN " | "
/// separators.
fn cohort_segments(co: &crate::model::CohortView) -> Vec<(String, Color)> {
    let mut segs: Vec<(String, Color)> = Vec::new();
    segs.push((
        if co.cached { "● ".into() } else { "○ ".into() },
        if co.cached { Color::Green } else { Color::DarkGrey },
    ));
    segs.push((
        format!("{:>4}  {:<14} ", co.count, co.key),
        // a not-yet-cached cohort's count + key are greyed (its venv isn't on disk yet)
        if !co.cached { Color::DarkGrey } else if co.key == "base" { Color::White } else { Color::Cyan },
    ));
    if co.families.is_empty() {
        segs.push(("(no families yet)".into(), Color::DarkGrey));
    } else {
        for (i, f) in co.families.iter().enumerate() {
            if i > 0 {
                segs.push((" | ".into(), Color::Cyan));
            }
            segs.push((f.name.clone(), fam_color(&f.status)));
        }
    }
    segs
}

/// The fontc-migration summary that heads the stats tab (port of the Python stats prefix): a counts
/// line, then the exact compiler/builder versions in use (M0). Returns the number of rows consumed.
fn render_stats_prefix(scr: &mut Screen, snap: &Snapshot, top: u16, w: u16) -> u16 {
    let mut row = top;
    let mut hdr = " fontc migration ".to_string();
    while hdr.chars().count() < (w as usize).saturating_sub(1) {
        hdr.push('-');
    }
    put(scr, row, 0, &hdr, Color::White, w);
    row += 1;
    let g = |k: &str| snap.migration.get(k).copied().unwrap_or(0);
    let mut line = format!(
        "builder3(M5: Python-free) {}   fontc {}   fontmake-fallback(blockers) {}   fontmake-only {}",
        g("builder3"), g("fontc"), g("fontmake_fallback"), g("fontmake_only")
    );
    if g("both_identical") > 0 || g("both_differ") > 0 {
        line += &format!("   both id {}/diff {}", g("both_identical"), g("both_differ"));
    }
    put(scr, row, 1, &line, Color::Green, w);
    row += 1;
    if !snap.tooling.is_empty() {
        let t: Vec<String> = snap.tooling.iter().map(|(k, v)| format!("{} → {}", k, v)).collect();
        put(scr, row, 1, &format!("compilers in use:  {}", t.join("   ")), Color::Cyan, w);
        row += 1;
    }
    if !snap.builders.is_empty() {
        let b: Vec<String> = snap.builders.iter().map(|(k, v)| format!("{} → {}", k, v)).collect();
        put(scr, row, 1, &format!("builders in use:   {}", b.join("   ")), Color::Cyan, w);
        row += 1;
    }
    row += 2; // blank gap before the timing sections (matches the Python `row += 2`)
    row - top
}

fn render_archive(scr: &mut Screen, snap: &Snapshot, top: u16, avail: u16, w: u16) {
    let a = &snap.archive;
    let unreachable = a.recent.iter().filter(|r| r.status == "failed").count();
    put(scr, top, 0, &format!(" {} repos mirrored on disk", a.total), Color::Cyan, w);
    put(scr, top + 1, 0, &format!("  {} cloning now   {} queued   {} unreachable",
        a.active.len(), a.pending_total, unreachable), Color::Grey, w);
    let mut row = top + 3;

    // multi-column grid: cloning-now (yellow) · recently-archived (green) · queued-next (cyan), to
    // fit as many repo slugs on screen as possible. Each section is a labelled colour block.
    let colw: usize = 30;
    let cols = (w as usize / colw).max(1);
    let grid = |scr: &mut Screen, row: &mut u16, items: &[String], color: Color, label: &str, maxrows: u16| {
        if items.is_empty() || maxrows == 0 { return; }
        put(scr, *row, 0, label, color, w);
        *row += 1;
        let mut shown = 0;
        let cap = (maxrows as usize - 1).saturating_mul(cols);
        for chunk in items.iter().take(cap).collect::<Vec<_>>().chunks(cols) {
            let line: String = chunk.iter().map(|s| format!("{:<width$}", trunc(s, colw - 1), width = colw)).collect();
            put(scr, *row, 1, &line, color, w);
            *row += 1;
            shown += chunk.len();
        }
        if items.len() > shown {
            put(scr, *row, 1, &format!("… +{} more", items.len() - shown), Color::DarkGrey, w);
            *row += 1;
        }
        *row += 1;
    };
    let budget = avail.saturating_sub(3);
    let recent_added: Vec<String> = a.recent.iter().filter(|r| r.status != "failed").map(|r| r.repo.clone()).collect();
    let unreach: Vec<String> = a.recent.iter().filter(|r| r.status == "failed")
        .map(|r| format!("{} ({})", r.repo, trunc(&r.reason, 16))).collect();
    grid(scr, &mut row, &a.active, Color::Yellow, " cloning now", budget / 4);
    grid(scr, &mut row, &recent_added, Color::Green, " recently archived (last 30 min)", budget / 3);
    grid(scr, &mut row, &a.pending, Color::Cyan, " queued next", budget / 3);
    grid(scr, &mut row, &unreach, Color::Red, " unreachable (git reason)", budget / 4);
    if a.active.is_empty() && a.recent.is_empty() && a.pending.is_empty() {
        put(scr, row, 1, "(archive idle — nothing being mirrored)", Color::DarkGrey, w);
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n.saturating_sub(1)).collect::<String>() + "…" }
}

/// The unified Configuration tab — a faithful port of the Python `view == "config"` renderer: a
/// scrollable list of visible schema fields (label + value + change/restart tag), an action-button
/// row, then the auto-relaxed-deps and applied-live-changes sections.
fn render_config(scr: &mut Screen, snap: &Snapshot, ui: &Ui, top: u16, bottom: u16, w: u16) {
    const VC: u16 = 36; // value column
    let pre_build = ui.setup || snap.pre_build;
    let mut row = top;
    let title = if pre_build {
        " Configuration — set up your build "
    } else {
        " Configuration — edit settings (live where possible) "
    };
    let mut hdr = title.to_string();
    while hdr.chars().count() < (w as usize).saturating_sub(1) {
        hdr.push('-');
    }
    put(scr, row, 0, &hdr, Color::White, w);
    row += 1;

    let vis = cfg_visible(&ui.cfg_fields);
    let actions = cfg_actions(ui.setup);
    let cf = &snap.config;
    let vals = cfg_typed(&ui.cfg_fields);

    // scroll the fields if they'd overflow into the reserved panel rows (keep the active one visible)
    let mut field_budget = bottom.saturating_sub(row + 2).max(1) as usize;
    field_budget = field_budget.saturating_sub(4).max(1); // reserve rows for up to 4 sub-section headers
    let mut fstart = 0usize;
    if vis.len() > field_budget {
        let afield = ui.cfg_active.min(vis.len().saturating_sub(1));
        fstart = afield.saturating_sub(field_budget.saturating_sub(1)).min(vis.len() - field_budget);
        if fstart > 0 {
            put(scr, row, 1, &format!("  ↑ {} more", fstart), Color::DarkGrey, w);
            row += 1;
            field_budget -= 1;
        }
    }
    let mut prev_group = "";
    for idx in fstart..vis.len().min(fstart + field_budget) {
        let f = &ui.cfg_fields[vis[idx]];
        let g = cfg_group(f.key);
        if g != prev_group {
            prev_group = g;
            if row + 1 < bottom {
                let mut hd = format!("─ {} ", g);
                while hd.chars().count() < (w as usize).saturating_sub(2) { hd.push('─'); }
                put(scr, row, 1, &hd, Color::DarkGrey, w);
                row += 1;
            }
        }
        let active = ui.cfg_active == idx;
        let editable = cfg_editable(ui.setup, f);
        let valstr = match &f.kind {
            CfgKind::Bool => if f.bval { "[x]".to_string() } else { "[ ]".to_string() },
            CfgKind::Choice(_) => format!("‹ {} ›", f.value),
            _ => f.value.clone(),
        };
        let mut tag = String::new();
        if !pre_build {
            let changed = vals.get(f.key) != cf.get(f.key);
            if f.live {
                if changed { tag = "  *changed".into(); }
            } else if cfg_persistable(f.key) {
                // saved to disk on apply; takes effect on the next run / --export-deb
                tag = if changed { "  *next run".into() } else { "  (next run)".into() };
            } else {
                tag = "  (start-only)".into(); // can't change without a fresh build invocation
            }
        }
        let lab_color = if active { Color::White } else if editable { Color::Grey } else { Color::DarkGrey };
        put(scr, row, 1, &format!("{}{}", if active { "▸ " } else { "  " }, f.label), lab_color, w);
        put_rev(scr, row, VC, &format!("{}{}", valstr, tag), if editable { Color::Yellow } else { Color::DarkGrey }, active);
        // a visible block caret on an active editable text/number field
        if active && editable && matches!(f.kind, CfgKind::Path | CfgKind::Step { .. }) {
            let cx = VC + f.caret.min(f.value.chars().count()) as u16;
            let ch = f.value.chars().nth(f.caret).unwrap_or(' ');
            put_rev(scr, row, cx, &ch.to_string(), Color::White, true);
        }
        row += 1;
    }
    if vis.len() > fstart + field_budget {
        put(scr, row, 1, &format!("  ↓ {} more", vis.len() - fstart - field_budget), Color::DarkGrey, w);
        row += 1;
    }

    // action button(s): ▶ Start build / Cancel (setup) or ✓ apply changes (live)
    let brow = (row + 1).min(bottom.saturating_sub(1));
    let mut bx = 2u16;
    for (ai, lbl) in actions.iter().enumerate() {
        let active = ui.cfg_active == vis.len() + ai;
        let s = format!(" {} ", lbl);
        put_rev(scr, brow, bx, &s, Color::White, active);
        bx += s.chars().count() as u16 + 4;
    }
    row = brow + 2;
    if !snap.dep_relaxations.is_empty() && row < bottom.saturating_sub(1) {
        let mut h = " auto-fixed dependencies (no manual pinning needed) ".to_string();
        while h.chars().count() < (w as usize).saturating_sub(1) { h.push('-'); }
        put(scr, row, 0, &h, Color::White, w);
        row += 1;
        for l in &snap.dep_relaxations {
            if row >= bottom.saturating_sub(1) { break; }
            put(scr, row, 1, l, Color::Yellow, w);
            row += 1;
        }
    }
    if !snap.control_log.is_empty() && row < bottom.saturating_sub(1) {
        let mut h = " applied live changes ".to_string();
        while h.chars().count() < (w as usize).saturating_sub(1) { h.push('-'); }
        put(scr, row, 0, &h, Color::White, w);
        row += 1;
        for l in snap.control_log.iter().rev() {
            if row >= bottom.saturating_sub(1) { break; }
            put(scr, row, 1, l, Color::Green, w);
            row += 1;
        }
    }
}

/// Per-field help text for the config tab status panel — mirrors the Python FIELD_HELP map.
fn field_help(key: &str) -> &str {
    match key {
        "source" => "where the worklist comes from: google/fonts METADATA, or every mirror in the archive",
        "google_fonts" => "path to a google/fonts clone (cloned shallow if absent)",
        "archive" => "the bare-mirror repo archive (append-only; never deleted)",
        "build_dir" => "where all build assets go (out/ venvs/ logs/) — never inside a repo",
        "backend" => "auto = fontc first then fall back to fontmake · fontc/fontmake = that one · both = build & compare",
        "orchestrator" => "auto = prefer builder3 (Rust), fall back to builder2 · builder3/builder2 = that one only",
        "fontc_bin" => "explicit fontc binary — empty = auto (provisioned pin / detected)",
        "auto_provision" => "cargo-install the pinned fontc + builder3 when absent (zero-setup toolchain)",
        "auto_upgrade" => "re-attempt fontmake/builder2-built families at better rungs (once per pin); a failed upgrade keeps the existing result + binaries",
        "jobs" => "how many families build in parallel",
        "percent" => "build only this % of the library (evenly-spaced sample); raise it live to build more",
        "timeout" => "per-build timeout in seconds (0 = never time out)",
        "populate_archive" => "mirror any missing upstream repos into the archive while building",
        "manage_venvs" => "create & share one venv per dependency cohort",
        "retry_failed" => "also re-attempt families that failed with genuine build errors (fixable causes — broken venvs, transient fetches — are always retried)",
        "compare" => "sha256-compare each built font to the shipped one (metadata source only)",
        "fontspector_qa" => "run fontspector QA asynchronously on each successfully-built family (niced; results in the fontspector tab)",
        _ => "edit with ←/→ or type",
    }
}

/// A short, context-sensitive description of the focused item for the always-on status panel —
/// a faithful port of the Python `_focus_info` (returns 1-3 lines), dispatched on the FOCUSED
/// section's detail-view tag so it follows ←/→ section navigation just like Python.
fn focus_info(snap: &Snapshot, ui: &Ui) -> Vec<String> {
    let sel = ui.sel;
    // config + archive are single custom views (no sections) — keep their dedicated info
    match TABS[ui.tab] {
        "config" => {
            let vis = cfg_visible(&ui.cfg_fields);
            if let Some(&fi) = vis.get(ui.cfg_active) {
                let f = &ui.cfg_fields[fi];
                return vec![format!(" {} — {}", f.label, field_help(f.key))];
            }
            let ai = ui.cfg_active.saturating_sub(vis.len());
            let help = match cfg_actions(ui.setup).get(ai).copied().unwrap_or("") {
                "▶ Start build" => "▶ launch the build with these settings",
                "Cancel" => "discard and exit — nothing is built",
                "✓ apply changes" => "apply the edited live settings to the running build now",
                _ => "",
            };
            return vec![format!(" {}", help)];
        }
        "archive" => {
            return snap.archive.pending.get(sel)
                .map(|r| vec![format!(" + {} — queued to be mirrored into the archive (a fresh bare clone)", r)])
                .unwrap_or_default();
        }
        _ => {}
    }
    let secs = sections_for(snap, ui.tab, ui.fc_sel);
    let dview = match secs.get(ui.section) { Some(s) => s.dview, None => return vec![] };
    match dview {
        "overview" => snap.tasks.get(sel).map(|t| {
            vec![format!(" {}: {}{}", t.name, t.status, if t.detail.is_empty() { String::new() } else { format!(" — {}", t.detail) })]
        }).unwrap_or_default(),
        "queue" => snap.queued_list.get(sel).map(|q| {
            let why = match q.kind.as_str() {
                "retry" => "re-attempt after a previous build failure",
                "rebuild" => "rebuild of a family that already built (--rebuild / [R])",
                "upgrade" => "auto-upgrade: built with a lower rung (e.g. fontmake) — re-attempting builder3/fontc; the existing result is kept if this fails",
                _ => "a fresh target — never built before",
            };
            vec![format!(" queued: {}  —  {}", q.slug, q.kind), format!("   {}", why)]
        }).unwrap_or_default(),
        "cohorts" => snap.cohorts.get(sel).map(|c| {
            let l1 = match c.requirements.lines().next() {
                Some(r) => format!(" cohort {}: {} families — needs {}", c.key, c.count, r),
                None => format!(" cohort {}: {} families (base — no extra requirements)", c.key, c.count),
            };
            let names: Vec<&str> = c.families.iter().map(|f| f.name.as_str()).collect();
            let l2 = if names.is_empty() { "   (none assigned yet)".to_string() } else { format!("   {}", names.join(" | ")) };
            vec![l1, l2]
        }).unwrap_or_default(),
        "built" => snap.built_recent.get(sel).map(|b| {
            let pv = prov_str(&b.compiler_version, &b.backend, &b.builder_version);
            let pv = if pv.is_empty() { "?".to_string() } else { pv };
            vec![format!(" {} ✓ built with {} — {}, vs shipped: {}", b.slug, pv, human(b.bytes),
                if b.compare.is_empty() { "not compared" } else { &b.compare })]
        }).unwrap_or_default(),
        "failures" => snap.failures_recent.get(sel).map(|f| {
            let pv = prov_str(&f.compiler_version, &f.backend, &f.builder_version);
            let head = if pv.is_empty() { format!(" {}  FAILED:", f.slug) } else { format!(" {}  FAILED [{}]:", f.slug, pv) };
            vec![head, format!("   {}", f.error)]
        }).unwrap_or_default(),
        "failcat" => snap.fail_categories.get(sel).map(|fc| {
            let l1 = format!(" {} families failed: {}", fc.count, fc.cat);
            if fc.families.is_empty() {
                vec![l1, format!("   → {}", fc.hint)]
            } else {
                let shown: Vec<&str> = fc.families.iter().take(6).map(|s| s.as_str()).collect();
                let more = if fc.families.len() > 6 { " …" } else { "" };
                vec![l1, format!("   {}{}", shown.join(", "), more), format!("   → {}", fc.hint)]
            }
        }).unwrap_or_default(),
        "history" => snap.failure_history.get(sel).map(|h| {
            let pv = prov_str(&h.compiler_version, &h.backend, &h.builder_version);
            let head = if pv.is_empty() { format!(" {}  —  {}", h.slug, h.cause) } else { format!(" {}  —  {}  [{}]", h.slug, h.cause, pv) };
            vec![head, format!("   {}", h.error)]
        }).unwrap_or_default(),
        "stats" => {
            let mut ops: Vec<(&String, &crate::model::OpStat)> = snap.op_stats.iter().collect();
            ops.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap_or(std::cmp::Ordering::Equal));
            ops.get(sel).map(|(op, s)| vec![format!(" {}: total {:.1}s · n {} · mean {:.2}s · max {:.1}s", op, s.total, s.count, s.mean, s.max)]).unwrap_or_default()
        }
        "fscheck" => snap.fontspector.as_ref().and_then(|fs| fs.per_check.get(sel)).map(|c| {
            let f = c.counts.fail + c.counts.fatal + c.counts.error;
            vec![format!(" {} — {}", c.id, c.title), format!("   {} fail · {} warn · {} pass · {} skip  (↵ for the affected families)", f, c.counts.warn, c.counts.pass, c.counts.skip)]
        }).unwrap_or_default(),
        "fsfamily" => snap.fontspector.as_ref().and_then(|fs| fs.per_family.get(sel)).map(|f| {
            let fc = f.counts.fail + f.counts.fatal + f.counts.error;
            vec![format!(" {}  [{}]", f.slug, f.worst), format!("   {} fail · {} warn · {} pass  (↵ for every check)", fc, f.counts.warn, f.counts.pass)]
        }).unwrap_or_default(),
        _ => vec![],
    }
}

/// Last `n` lines of a per-family log file (for the failure/built/building detail overlay).
fn read_log_tail(path: &str, n: usize) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => {
            let lines: Vec<&str> = t.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].iter().map(|s| s.to_string()).collect()
        }
        Err(_) => Vec::new(),
    }
}

/// Word-wrap one logical line to `width` columns, preserving a leading-space indent and hard-breaking
/// over-long words. So a long error message renders as multiple lines instead of overflowing.
fn wrap_line(s: &str, width: usize) -> Vec<String> {
    if width == 0 || s.chars().count() <= width {
        return vec![s.to_string()];
    }
    let indent: String = s.chars().take_while(|c| *c == ' ').collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let candidate = if cur.is_empty() { word.to_string() } else { format!("{} {}", cur, word) };
        if candidate.chars().count() <= width {
            cur = candidate;
        } else {
            if !cur.is_empty() {
                out.push(cur);
            }
            cur = format!("{}{}", indent, word);
            while cur.chars().count() > width {
                out.push(cur.chars().take(width).collect());
                cur = format!("{}{}", indent, cur.chars().skip(width).collect::<String>().trim_start());
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Build the full detail content for the selected list item — a faithful port of the Python
/// `_detail_lines` (incl. reading the per-family log tail). Captured ONCE when the overlay opens.
fn build_detail(snap: &Snapshot, tab: usize, section: usize, sel: usize, fc_sel: usize, build_dir: &std::path::Path) -> Vec<String> {
    let mut o: Vec<String> = Vec::new();
    // archive/config are single custom views; every other tab dispatches on the FOCUSED section's
    // detail-view tag — so the right detail opens for whichever section ←/→ has focused.
    let dview: &str = match TABS[tab] {
        "config" => "config",
        "archive" => "archive",
        _ => sections_for(snap, tab, fc_sel).get(section).map(|s| s.dview).unwrap_or(""),
    };
    match dview {
        "reset" => {
            if let Some(p) = snap.reset_portions.get(sel) {
                o.push(format!(" reset: {}", p.label));
                o.push(format!("   on disk now: {}", crate::util::human(p.bytes)));
                o.push(format!("   {}", p.hint));
                o.push("   the repo archive, google/fonts clone and build RESULTS are never touched".into());
                o.push(String::new());
                o.push("   ▶ press D to DELETE now — items in use by a running build are kept, the rest are deleted".into());
            }
            return o;
        }
        "failures" => {
            // rows may come from a selected cause's family list (not failures_recent), so
            // resolve the slug via the section's keys, then look it up in recent → history.
            let slug = sections_for(snap, tab, fc_sel).get(section).and_then(|s| s.keys.get(sel).cloned());
            if let Some(slug) = slug {
                if let Some(f) = snap.failures_recent.iter().find(|f| f.slug == slug) {
                    if !f.rebuild_note.is_empty() {
                        o.push(format!("⟳ REBUILD PENDING — {}", f.rebuild_note));
                        o.push(String::new());
                    }
                    o.push(format!("Failed: {}", f.slug));
                    o.push(format!("provenance: {}", prov_str(&f.compiler_version, &f.backend, &f.builder_version)));
                    o.push(format!("rebuild: gflib-build --only {} --rebuild --yes", f.slug));
                    o.push(String::new());
                    o.push("error:".into());
                    o.push(format!("  {}", f.error));
                    o.push(String::new());
                    o.push(format!("log: {}", if f.log.is_empty() { "(none)".into() } else { f.log.clone() }));
                    o.push("log tail:".into());
                    for ln in read_log_tail(&f.log, 120) {
                        o.push(format!("  {}", ln));
                    }
                } else if let Some(h) = snap.failure_history.iter().rev().find(|h| h.slug == slug) {
                    // not in the recent window — render from the persistent failure history
                    if let Some(note) = crate::classify::rebuild_pending_note("failed", "", &h.error) {
                        o.push(format!("⟳ REBUILD PENDING — {}", note));
                        o.push(String::new());
                    }
                    o.push(format!("Failed: {}  (from failure history)", h.slug));
                    o.push(format!("cause: {}", h.cause));
                    o.push(format!("provenance: {}", prov_str(&h.compiler_version, &h.backend, &h.builder_version)));
                    o.push(format!("rebuild: gflib-build --only {} --rebuild --yes", h.slug));
                    o.push(String::new());
                    o.push("error:".into());
                    o.push(format!("  {}", h.error));
                } else {
                    o.push(format!("Failed: {}", slug));
                    o.push("(no recorded error details for this family in the current snapshot)".into());
                }
            }
        }
        "built" => {
            if let Some(b) = snap.built_recent.get(sel) {
                o.push(format!("Built: {}", b.slug));
                o.push(format!("backend: {}", if b.backend.is_empty() { "?" } else { &b.backend }));
                o.push(format!("output size: {}", human(b.bytes)));
                o.push(format!("vs shipped: {}", if b.compare.is_empty() { "(not compared)" } else { &b.compare }));
                o.push(format!("provenance: {}", prov_str(&b.compiler_version, &b.backend, &b.builder_version)));
                if !b.python_version.is_empty() {
                    o.push(format!("python: {}", b.python_version));
                }
                o.push(format!("fonts: {}", build_dir.join("out").join(b.slug.replace('/', "__")).display()));
                o.push(format!("rebuild: gflib-build --only {} --rebuild --yes", b.slug));
                if !b.log.is_empty() {
                    o.push(String::new());
                    o.push("log tail:".into());
                    for ln in read_log_tail(&b.log, 60) {
                        o.push(format!("  {}", ln));
                    }
                }
            }
        }
        "package" => {
            let slug = sections_for(snap, tab, fc_sel).get(section).and_then(|s| s.keys.get(sel).cloned());
            if let Some(slug) = slug {
                o.push(format!("Package: {}", slug));
                if let Some(b) = snap.packages.iter().find(|b| b.slug == slug) {
                    let st = if !b.deb_status.is_empty() {
                        b.deb_status.clone()
                    } else if b.packaged {
                        "drafted".into()
                    } else {
                        "draftable (built, not yet drafted)".into()
                    };
                    o.push(format!("deb status: {}", st));
                }
                o.push(String::new());
                for ln in crate::deb::package_metadata(build_dir, &slug).lines() {
                    o.push(ln.to_string());
                }
            }
        }
        "tool" => {
            let name = sections_for(snap, tab, fc_sel).get(section).and_then(|s| s.keys.get(sel).cloned());
            if let Some(name) = name {
                if let Some(t) = snap.tool_packages.iter().find(|t| t.name == name) {
                    o.push(format!("Build-tool package: {}", t.name));
                    o.push(format!(
                        "language: {}   kind: {}   packaged: {}",
                        t.lang, t.kind, if t.packaged { "yes" } else { "no" }
                    ));
                    let more = if t.family_list.len() < t.families {
                        format!(" (showing first {})", t.family_list.len())
                    } else {
                        String::new()
                    };
                    o.push(format!("required by {} families{}", t.families, more));
                    o.push(String::new());
                    o.push("families:".into());
                    for fam in &t.family_list {
                        o.push(format!("  {}", fam));
                    }
                }
            }
        }
        "queue" => {
            if let Some(q) = snap.queued_list.get(sel) {
                o.push(format!("Queued family: {}", q.slug));
                o.push(format!("kind: {}", q.kind));
                o.push(String::new());
                o.push(match q.kind.as_str() {
                    "retry" => "Re-attempt after a previous build FAILURE (its cause may now be fixable: a rebuilt venv, a retried clone, a code fix, …).",
                    "rebuild" => "Rebuild of a family that already built successfully — forced by --rebuild or by pressing [R] on a built family.",
                    _ => "A fresh target — this family has never been built.",
                }.to_string());
            }
        }
        "cohorts" => {
            if let Some(c) = snap.cohorts.get(sel) {
                o.push(format!("Cohort: {}", c.key));
                o.push(format!("families: {}", c.count));
                // per-status tally so the overlay summarizes the cohort's progress at a glance
                let tally = |st: &str| c.families.iter().filter(|f| f.status == st).count();
                o.push(format!("status: {} built · {} failed · {} building · {} queued",
                    tally("built"), tally("failed"), tally("building"),
                    c.families.iter().filter(|f| !matches!(f.status.as_str(), "built" | "failed" | "building")).count()));
                o.push(String::new());
                o.push("family names (with build status):".into());
                if c.families.is_empty() {
                    o.push("  (none assigned yet)".into());
                }
                for f in &c.families {
                    o.push(format!("  {} [{}]", f.name, f.status));
                }
                o.push(String::new());
                o.push("requirements:".into());
                if c.requirements.is_empty() {
                    o.push("  (none — the 'base' cohort has no requirements file)".into());
                }
                for r in c.requirements.lines() {
                    o.push(format!("  {}", r));
                }
            }
        }
        "failcat" => {
            if let Some(fc) = snap.fail_categories.get(sel) {
                o.push(format!("Failure cause: {}", fc.cat));
                o.push(format!("families affected: {}", fc.count));
                o.push(String::new());
                o.push("affected families:".into());
                if fc.families.is_empty() {
                    o.push("  (none)".into());
                }
                for s in &fc.families {
                    o.push(format!("  {}", s));
                }
                o.push(String::new());
                o.push("what to do:".into());
                o.push(format!("  {}", fc.hint));
            }
        }
        "lintcat" => {
            if let Some(c) = snap.lint_categories.get(sel) {
                o.push(format!("lintian {}: {}", if c.severity == "E" { "error" } else { "warning" }, c.tag));
                o.push(format!("packages affected: {}", c.count));
                o.push(format!("lintian tag docs: https://lintian.debian.org/tags/{}", c.tag));
                o.push(String::new());
                o.push("affected packages:".into());
                if c.families.is_empty() {
                    o.push("  (none)".into());
                }
                for s in &c.families {
                    o.push(format!("  {}", s));
                }
            }
        }
        "history" => {
            if let Some(h) = snap.failure_history.get(sel) {
                o.push(format!("Failed (history): {}", h.slug));
                o.push(format!("cause: {}", h.cause));
                o.push(format!("provenance: {}", prov_str(&h.compiler_version, &h.backend, &h.builder_version)));
                o.push(format!("rebuild: gflib-build --only {} --rebuild --yes", h.slug));
                o.push(String::new());
                o.push("error:".into());
                o.push(format!("  {}", h.error));
            }
        }
        "overview" => {
            if let Some(t) = snap.tasks.get(sel) {
                o.push(format!("Pipeline task: {}", t.name));
                o.push(format!("status: {}", t.status));
                if t.total > 0 {
                    o.push(format!("progress: {}/{}", t.done, t.total));
                }
                if t.elapsed > 0.0 {
                    o.push(format!("elapsed: {}", hms(t.elapsed)));
                }
                if !t.detail.is_empty() {
                    o.push(format!("detail: {}", t.detail));
                }
            }
        }
        "stats" => {
            // operation timing (the focused 'Operation timing' section, sorted by total desc)
            let mut ops: Vec<(&String, &crate::model::OpStat)> = snap.op_stats.iter().collect();
            ops.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((op, s)) = ops.get(sel) {
                o.push(format!("Operation: {}", op));
                o.push(format!("total: {:.1}s", s.total));
                o.push(format!("count: {}", s.count));
                o.push(format!("mean: {:.2}s", s.mean));
                o.push(format!("max: {:.1}s", s.max));
            }
        }
        "archive" => {
            if let Some(repo) = snap.archive.pending.get(sel) {
                o.push(format!("Queued to mirror: {}", repo));
                o.push(String::new());
                o.push("This upstream repo is not yet in the archive; it will be cloned (append-only) when --mirror-missing is on.".into());
            }
        }
        "fscheck" => {
            if let Some(c) = snap.fontspector.as_ref().and_then(|fs| fs.per_check.get(sel)) {
                let f = c.counts.fail + c.counts.fatal + c.counts.error;
                o.push(format!("Check: {}", c.id));
                o.push(format!("title: {}", c.title));
                o.push(format!("results: {} fail · {} warn · {} pass · {} skip · {} info", f, c.counts.warn, c.counts.pass, c.counts.skip, c.counts.info));
                o.push(String::new());
                o.push("FAIL families:".into());
                if c.fail_families.is_empty() { o.push("  (none)".into()); }
                for s in &c.fail_families { o.push(format!("  {}", s)); }
                o.push(String::new());
                o.push("WARN families:".into());
                if c.warn_families.is_empty() { o.push("  (none)".into()); }
                for s in &c.warn_families { o.push(format!("  {}", s)); }
            }
        }
        "fsfamily" => {
            if let Some(fam) = snap.fontspector.as_ref().and_then(|fs| fs.per_family.get(sel)) {
                o.push(format!("Family QA: {}", fam.slug));
                // read the family's full result for the per-check list (severity-sorted)
                let path = build_dir.join("fontspector").join(format!("{}.json", fam.slug.replace('/', "__")));
                let parsed = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
                if let Some(v) = parsed {
                    o.push(format!("fontspector: {}", v.get("fontspector_version").and_then(|x| x.as_str()).unwrap_or("?")));
                    let fc = fam.counts.fail + fam.counts.fatal + fam.counts.error;
                    o.push(format!("results: {} fail · {} warn · {} pass · {} skip", fc, fam.counts.warn, fam.counts.pass, fam.counts.skip));
                    o.push(String::new());
                    o.push("checks (worst first):".into());
                    let mut checks: Vec<(&str, &str, &str)> = v.get("checks").and_then(|c| c.as_array()).into_iter().flatten()
                        .map(|c| (
                            c.get("status").and_then(|x| x.as_str()).unwrap_or(""),
                            c.get("id").and_then(|x| x.as_str()).unwrap_or(""),
                            c.get("title").and_then(|x| x.as_str()).unwrap_or(""),
                        )).collect();
                    checks.sort_by(|a, b| sev_rank(b.0).cmp(&sev_rank(a.0)).then(a.1.cmp(b.1)));
                    for (st, id, title) in checks {
                        o.push(format!("  {:<6} {}  —  {}", st, id, title));
                    }
                } else {
                    o.push("(no stored result — run gflib-build --fontspector)".into());
                }
            }
        }
        _ => {} // config (edits in place, no overlay), phase-timing (dview ""), etc: no detail
    }
    o
}

/// True when `l` looks like a Python exception header (`SomeError: …` / `SomeException: …`).
fn is_exception(l: &str) -> bool {
    match l.find(':') {
        Some(i) => {
            let head = &l[..i];
            !head.is_empty()
                && head.chars().all(|c| c.is_alphanumeric())
                && (head.ends_with("Error") || head.ends_with("Exception"))
        }
        None => false,
    }
}

/// Per-line colour for a build-log line in the detail overlay (mirrors the web `logCls`).
fn log_color(l: &str) -> Color {
    if let Some(rest) = l.strip_prefix("[+") {
        let _ = rest;
        if l.contains("FAIL") {
            Color::Red
        } else if l.contains(" ok") || l.ends_with("ok") {
            Color::Green
        } else {
            Color::Cyan
        }
    } else if l.starts_with('#') || l.starts_with("=====") || (l.starts_with('[') && l.contains('/')) {
        Color::Cyan // meta / banners / [N/M] progress
    } else if l.starts_with("File \"") || (l.contains(['~', '^']) && l.chars().all(|c| matches!(c, '~' | '^' | ' '))) {
        Color::DarkGrey // traceback frames / caret lines
    } else if l.contains("Traceback") || l.contains("Command failed") || l.starts_with("FAILED")
        || l.contains("error[") || l.contains("error:") || l.contains("ERROR") || l.contains("panic")
        || is_exception(l)
    {
        Color::Red
    } else if l.contains("WARNING") || l.contains("warning:") || l.contains("WARN") {
        Color::Yellow
    } else if l.contains("Successfully") || l.contains("PASS") || l.contains(": ok") {
        Color::Green
    } else if l.starts_with("INFO:") || l.starts_with("DEBUG:") || l.contains("INFO") {
        Color::DarkGrey
    } else {
        Color::Grey
    }
}

fn render_detail(scr: &mut Screen, ui: &Ui, w: u16, h: u16) {
    // full-width overlay covering the body region (like the Python detail overlay), scrollable.
    let top = 5u16;
    let body_top = top + 1;
    let view = h.saturating_sub(body_top + 1).max(1) as usize; // leave the bottom row free
    let inner = (w.saturating_sub(3)).max(10) as usize;
    // word-wrap the captured logical lines, assigning each a colour: "header:" lines are cyan; lines
    // after a "log tail" header are syntax-highlighted as build-log output; everything else is white.
    let mut wrapped: Vec<(String, Color)> = Vec::new();
    let mut in_log = false;
    for l in &ui.detail_lines {
        if l.is_empty() {
            wrapped.push((String::new(), Color::White));
            continue;
        }
        let is_hdr = !l.starts_with(' ') && l.ends_with(':');
        if is_hdr {
            in_log = l.starts_with("log tail");
        }
        let color = if is_hdr {
            Color::Cyan
        } else if in_log {
            log_color(l.trim_start())
        } else {
            Color::White
        };
        for wl in wrap_line(l, inner) {
            wrapped.push((wl, color));
        }
    }
    let maxscroll = wrapped.len().saturating_sub(view);
    let ds = ui.dscroll.min(maxscroll);
    // clear the overlay region so the body underneath doesn't show through
    for r in top..h {
        put(scr, r, 0, &" ".repeat(w as usize), Color::Reset, w);
    }
    let hdr = " Details — [Esc/↵] back   [↑↓] scroll ";
    let pad = (w as usize).saturating_sub(hdr.chars().count());
    put(scr, top, 0, &format!("{}{}", hdr, "─".repeat(pad)), Color::Cyan, w);
    for (i, (wl, color)) in wrapped.iter().skip(ds).take(view).enumerate() {
        put(scr, body_top + i as u16, 1, wl, *color, w);
    }
    if maxscroll > 0 {
        let pos = format!(" {}–{}/{} ", ds + 1, (ds + view).min(wrapped.len()), wrapped.len());
        put(scr, h.saturating_sub(1), w.saturating_sub(pos.len() as u16 + 1), &pos, Color::DarkGrey, w);
    }
}

fn prov_str(cver: &str, backend: &str, bver: &str) -> String {
    let c = if cver.is_empty() { backend } else { cver };
    if bver.is_empty() {
        c.to_string()
    } else {
        format!("{} · {}", c, bver)
    }
}

fn scroll_start(sel: usize, view: usize, total: usize) -> usize {
    if total <= view || view == 0 {
        return 0;
    }
    if sel < view {
        0
    } else {
        (sel + 1 - view).min(total - view)
    }
}

#[cfg(test)]
mod tests {
    use super::water_fill;
    #[test]
    fn water_fill_shares_and_releases_surplus() {
        assert_eq!(water_fill(&[2, 3], 10), vec![2, 3]); // both fit -> exact
        let a = water_fill(&[1, 100], 10); // small section releases surplus to the large one
        assert_eq!(a[0], 1);
        assert_eq!(a[1], 9);
        let b = water_fill(&[50, 50], 10); // both want more than half -> shared, sum == avail
        assert_eq!(b[0] + b[1], 10);
        assert!(b[0] >= 4 && b[1] >= 4);
        assert_eq!(water_fill(&[5], 0), vec![0]); // no room
    }
}
