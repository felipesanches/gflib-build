//! The curses-style dashboard, ported to crossterm. Renders the same snapshot the Python TUI does:
//! a two-line header (cumulative elapsed + disk/jobs/backends), a phase/progress bar, arrow/Tab tabs
//! (config · overview · queue · cohorts · built · failures · stats · archive), a per-tab list with
//! ↑/↓ selection + ↵ detail overlay, an always-on status panel, and a footer. Live controls (pause,
//! retry, jobs/percent) are written to control.json — the same channel the web UI uses — so a live
//! build and an attached monitor behave identically.

use crate::model::{ControlSet, Snapshot};
use crate::monitor::Source;
use crate::util::{human, hms};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use std::io::{Stdout, Write};
use std::sync::Arc;
use std::time::Duration;

const TABS: [&str; 8] = [
    "config", "overview", "queue", "cohorts", "built", "failures", "stats", "archive",
];

// The live-editable config fields (R5): ←/→ adjusts the selected one and applies it to the running
// build via control.json (the same channel the web UI uses). Mirrors the Python config tab's live set.
const CONFIG_FIELDS: [&str; 5] = ["jobs", "percent", "backend", "compare", "paused"];
const BACKENDS: [&str; 4] = ["auto", "fontc", "fontmake", "both"];

/// Outcome of the TUI loop: the user quit, or pressed C to reconfigure (re-show setup).
pub enum TuiResult {
    Quit,
    Reconfigure,
}

struct Ui {
    tab: usize,
    sel: usize,
    detail: bool,
}

pub fn run(source: Arc<dyn Source>) -> std::io::Result<TuiResult> {
    let mut out = std::io::stdout();
    terminal::enable_raw_mode()?;
    queue!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    out.flush()?;

    let mut ui = Ui { tab: 1, sel: 0, detail: false };
    let res = loop {
        let snap = source.snapshot();
        render(&mut out, &snap, &ui, &*source)?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != event::KeyEventKind::Press && k.kind != event::KeyEventKind::Repeat {
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') => break TuiResult::Quit,
                    KeyCode::Char('c') | KeyCode::Char('C') => break TuiResult::Reconfigure,
                    KeyCode::Tab => {
                        ui.tab = (ui.tab + 1) % TABS.len();
                        ui.sel = 0;
                        ui.detail = false;
                    }
                    KeyCode::BackTab => {
                        ui.tab = (ui.tab + TABS.len() - 1) % TABS.len();
                        ui.sel = 0;
                        ui.detail = false;
                    }
                    KeyCode::Up => {
                        if ui.sel > 0 {
                            ui.sel -= 1;
                        }
                    }
                    KeyCode::Down => {
                        ui.sel = (ui.sel + 1).min(list_len(&snap, ui.tab).saturating_sub(1));
                    }
                    KeyCode::Left => {
                        if TABS[ui.tab] == "config" {
                            adjust_config(&snap, ui.sel, -1, &*source);
                        }
                    }
                    KeyCode::Right => {
                        if TABS[ui.tab] == "config" {
                            adjust_config(&snap, ui.sel, 1, &*source);
                        }
                    }
                    KeyCode::Char(' ') => {
                        if TABS[ui.tab] == "config" {
                            adjust_config(&snap, ui.sel, 1, &*source); // space toggles bools / steps
                        }
                    }
                    KeyCode::Enter => ui.detail = !ui.detail,
                    KeyCode::Esc => ui.detail = false,
                    KeyCode::Char('p') | KeyCode::Char('P') => {
                        source.control(&ControlSet {
                            paused: Some(!snap.paused),
                            ..Default::default()
                        });
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        if let Some(slug) = selected_slug(&snap, ui.tab, ui.sel) {
                            source.control(&ControlSet {
                                retry: Some(vec![slug]),
                                ..Default::default()
                            });
                        }
                    }
                    KeyCode::Char('+') => {
                        source.control(&ControlSet {
                            jobs: Some(snap.jobs + 1),
                            ..Default::default()
                        });
                    }
                    KeyCode::Char('-') => {
                        source.control(&ControlSet {
                            jobs: Some(snap.jobs.saturating_sub(1).max(1)),
                            ..Default::default()
                        });
                    }
                    _ => {
                        // allow Shift-Tab via modifiers on some terminals
                        if k.modifiers.contains(KeyModifiers::SHIFT) && k.code == KeyCode::Tab {
                            ui.tab = (ui.tab + TABS.len() - 1) % TABS.len();
                        }
                    }
                }
            }
        }
    };

    queue!(out, terminal::LeaveAlternateScreen, cursor::Show, ResetColor)?;
    out.flush()?;
    terminal::disable_raw_mode()?;
    Ok(res)
}

fn list_len(snap: &Snapshot, tab: usize) -> usize {
    match TABS[tab] {
        "config" => CONFIG_FIELDS.len(),
        "queue" => snap.queued_list.len(),
        "cohorts" => snap.cohorts.len(),
        "built" => snap.built_recent.len(),
        "failures" => snap.failures_recent.len(),
        "stats" => snap.fail_categories.len(),
        "archive" => snap.archive.pending.len(),
        _ => snap.building.len(),
    }
}

/// Read the selected config field, adjust it by `delta` (or toggle a bool / cycle a choice), and
/// apply it live to the running build via control.json.
fn adjust_config(snap: &Snapshot, field: usize, delta: i64, src: &dyn Source) {
    let cf = &snap.config;
    let getf = |k: &str| cf.get(k).and_then(|v| v.as_f64());
    let gets = |k: &str| cf.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let getb = |k: &str| cf.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let mut set = ControlSet::default();
    match CONFIG_FIELDS.get(field).copied().unwrap_or("") {
        "jobs" => set.jobs = Some(((snap.jobs as i64 + delta).max(1)) as usize),
        "percent" => {
            let cur = getf("percent").unwrap_or(snap.config.get("percent").and_then(|v| v.as_f64()).unwrap_or(100.0));
            set.percent = Some((cur + delta as f64 * 5.0).clamp(0.0, 100.0));
        }
        "backend" => {
            let cur = gets("backend");
            let i = BACKENDS.iter().position(|b| *b == cur).unwrap_or(0) as i64;
            let n = BACKENDS.len() as i64;
            let ni = ((i + delta) % n + n) % n;
            set.backend = Some(BACKENDS[ni as usize].to_string());
        }
        "compare" => set.compare = Some(!getb("compare")),
        "paused" => set.paused = Some(!snap.paused),
        _ => {}
    }
    src.control(&set);
}

fn selected_slug(snap: &Snapshot, tab: usize, sel: usize) -> Option<String> {
    match TABS[tab] {
        "failures" => snap.failures_recent.get(sel).map(|f| f.slug.clone()),
        "built" => snap.built_recent.get(sel).map(|b| b.slug.clone()),
        "queue" => snap.queued_list.get(sel).map(|q| q.slug.clone()),
        _ => None,
    }
}

fn put(out: &mut Stdout, row: u16, col: u16, text: &str, color: Color, width: u16) {
    let mut s: String = text.chars().take(width.saturating_sub(col) as usize).collect();
    // strip control chars
    s = s.chars().filter(|c| !c.is_control()).collect();
    let _ = queue!(out, cursor::MoveTo(col, row), SetForegroundColor(color), Print(s), ResetColor);
}

fn render(out: &mut Stdout, snap: &Snapshot, ui: &Ui, src: &dyn Source) -> std::io::Result<()> {
    let (w, h) = terminal::size().unwrap_or((100, 40));
    queue!(out, terminal::Clear(terminal::ClearType::All))?;

    // ---- header ----
    let mode = if src.is_live() { "live" } else if snap.daemon_alive { "monitor" } else { "stopped" };
    let title = format!(
        " Google Fonts library build — Rust port [{}]{}",
        mode,
        if snap.paused { "  [PAUSED]" } else { "" }
    );
    put(out, 0, 0, &title, Color::White, w);
    put(out, 0, w.saturating_sub(18), &format!("elapsed {}", hms(snap.elapsed)), Color::Grey, w);

    let bld = snap.disk_build_total;
    let arc = snap.disk_archive_total;
    // Always spell out both components — no ambiguous "(build dir)".
    let disk = if snap.disk_archive_nested {
        format!("disk used {} (build + nested archive, all included)", human(bld))
    } else {
        format!("disk used {} (build {} + archive {})", human(bld + arc), human(bld), human(arc))
    };
    put(
        out,
        1,
        0,
        &format!(
            " {}  free {}  jobs {}  fontc {}/fontmake {}",
            disk, human(snap.disk_free), snap.jobs, snap.backends.fontc, snap.backends.fontmake
        ),
        Color::Cyan,
        w,
    );

    // ---- phase + progress ----
    let c = &snap.counts;
    let processed = c.built + c.failed + c.skipped;
    let in_scope = processed + c.queued + c.building;
    let pct = if in_scope > 0 { processed * 100 / in_scope } else { 0 };
    put(
        out,
        2,
        0,
        &format!(
            " Phase: {}   built {}  failed {}  building {}  queued {}",
            snap.phase, c.built, c.failed, c.building, c.queued
        ),
        Color::Green,
        w,
    );
    let barw = w.saturating_sub(8) as usize;
    let fill = barw * pct / 100;
    let bar: String = std::iter::repeat('#').take(fill).chain(std::iter::repeat('-').take(barw - fill)).collect();
    put(out, 3, 0, &format!(" [{}] {:>3}%", bar, pct), Color::Cyan, w);

    // ---- tab bar ----
    let mut tabline = String::from(" ");
    for (i, t) in TABS.iter().enumerate() {
        if i == ui.tab {
            tabline.push_str(&format!("[{}] ", t));
        } else {
            tabline.push_str(&format!(" {}  ", t));
        }
    }
    put(out, 4, 0, &tabline, Color::Yellow, w);

    // ---- pinned now-building (on every tab) ----
    let body_top = 6u16;
    let footer_row = h.saturating_sub(2);
    let panel_row = h.saturating_sub(3);
    let mut row = body_top;
    if !snap.building.is_empty() && TABS[ui.tab] != "overview" {
        put(out, row, 0, &format!(" Now building ({})", snap.building.len()), Color::DarkYellow, w);
        row += 1;
        for b in snap.building.iter().take(3) {
            put(
                out,
                row,
                1,
                &format!("w{} {:<40} {:>6}  {}", b.worker, b.slug, hms(b.dur), b.note),
                Color::Grey,
                w,
            );
            row += 1;
        }
        row += 1;
    }

    // ---- body per tab ----
    let avail = panel_row.saturating_sub(row);
    match TABS[ui.tab] {
        "overview" => render_overview(out, snap, row, avail, w),
        "queue" => render_list_simple(
            out,
            row,
            avail,
            w,
            ui.sel,
            "Queue",
            snap.queued_list.iter().map(|q| format!("{:<48} {}", q.slug, q.kind)).collect(),
        ),
        "cohorts" => render_list_simple(
            out,
            row,
            avail,
            w,
            ui.sel,
            "Cohorts",
            snap.cohorts.iter().map(|c| format!("{:<20} {} families", c.key, c.count)).collect(),
        ),
        "built" => render_built(out, snap, row, avail, w, ui.sel),
        "failures" => render_failures(out, snap, row, avail, w, ui.sel),
        "stats" => render_stats(out, snap, row, avail, w),
        "archive" => render_archive(out, snap, row, avail, w),
        _ => render_config(out, snap, ui.sel, row, w),
    }

    // ---- status panel ----
    put(out, panel_row, 0, &"─".repeat(w as usize), Color::DarkGrey, w);
    let panel = status_panel(snap, ui);
    put(out, panel_row + 0, 0, "", Color::Grey, w);
    put(out, panel_row, 0, &panel, Color::White, w);

    // ---- footer ----
    put(
        out,
        footer_row,
        0,
        " [Tab/⇧Tab]tabs  [↑↓]item  [↵]details  [p]ause  [R]etry  [+/-]jobs  [C]onfig  [q]uit",
        Color::DarkGrey,
        w,
    );

    // detail overlay
    if ui.detail {
        render_detail(out, snap, ui, w, h);
    }
    out.flush()
}

fn render_overview(out: &mut Stdout, snap: &Snapshot, top: u16, avail: u16, w: u16) {
    let mut row = top;
    put(out, row, 0, &format!(" Now building ({})", snap.building.len()), Color::DarkYellow, w);
    row += 1;
    for b in snap.building.iter().take((avail / 3).max(1) as usize) {
        put(out, row, 1, &format!("w{} {:<44} {:>6} {}", b.worker, b.slug, hms(b.dur), b.note), Color::Grey, w);
        row += 1;
    }
    row += 1;
    put(out, row, 0, &format!(" Recent failures ({})", snap.failures_recent.len()), Color::Red, w);
    row += 1;
    let end = avail.saturating_sub(row - top);
    for f in snap.failures_recent.iter().take(end as usize) {
        put(out, row, 1, &format!("{:<36} {}", f.slug, f.error), Color::Grey, w);
        row += 1;
    }
}

fn render_list_simple(
    out: &mut Stdout,
    top: u16,
    avail: u16,
    w: u16,
    sel: usize,
    title: &str,
    items: Vec<String>,
) {
    put(out, top, 0, &format!(" {} ({})", title, items.len()), Color::Cyan, w);
    let mut row = top + 1;
    let start = scroll_start(sel, avail as usize - 1, items.len());
    for (i, it) in items.iter().enumerate().skip(start).take(avail as usize - 1) {
        let color = if i == sel { Color::Yellow } else { Color::Grey };
        let marker = if i == sel { "▸ " } else { "  " };
        put(out, row, 0, &format!("{}{}", marker, it), color, w);
        row += 1;
    }
}

fn render_built(out: &mut Stdout, snap: &Snapshot, top: u16, avail: u16, w: u16, sel: usize) {
    put(
        out,
        top,
        0,
        &format!(" Built — successes ({})  slug · compiler · builder · size", snap.built_recent.len()),
        Color::Green,
        w,
    );
    let mut row = top + 1;
    let start = scroll_start(sel, avail as usize - 1, snap.built_recent.len());
    for (i, b) in snap.built_recent.iter().enumerate().skip(start).take(avail as usize - 1) {
        let color = if i == sel { Color::Yellow } else { Color::Green };
        let prov = prov_str(&b.compiler_version, &b.backend, &b.builder_version);
        let marker = if i == sel { "▸ " } else { "  " };
        put(
            out,
            row,
            0,
            &format!("{}{:<30} {:<34} {}", marker, b.slug, prov, human(b.bytes)),
            color,
            w,
        );
        row += 1;
    }
}

fn render_failures(out: &mut Stdout, snap: &Snapshot, top: u16, avail: u16, w: u16, sel: usize) {
    put(out, top, 0, &format!(" Failures ({})", snap.failures_recent.len()), Color::Red, w);
    let mut row = top + 1;
    let start = scroll_start(sel, avail as usize - 1, snap.failures_recent.len());
    for (i, f) in snap.failures_recent.iter().enumerate().skip(start).take(avail as usize - 1) {
        let color = if i == sel { Color::Yellow } else { Color::Grey };
        let marker = if i == sel { "▸ " } else { "  " };
        put(out, row, 0, &format!("{}{:<32} {}", marker, f.slug, f.error), color, w);
        row += 1;
    }
}

fn render_stats(out: &mut Stdout, snap: &Snapshot, top: u16, avail: u16, w: u16) {
    let mut row = top;
    put(out, row, 0, " Migration", Color::Green, w);
    row += 1;
    for (k, v) in &snap.migration {
        put(out, row, 1, &format!("{:<22} {}", k, v), Color::Grey, w);
        row += 1;
    }
    row += 1;
    if !snap.tooling.is_empty() {
        let t: Vec<String> = snap.tooling.iter().map(|(k, v)| format!("{} → {}", k, v)).collect();
        put(out, row, 1, &format!("compilers in use:  {}", t.join("   ")), Color::Cyan, w);
        row += 1;
    }
    if !snap.builders.is_empty() {
        let b: Vec<String> = snap.builders.iter().map(|(k, v)| format!("{} → {}", k, v)).collect();
        put(out, row, 1, &format!("builders in use:   {}", b.join("   ")), Color::Cyan, w);
        row += 1;
    }
    row += 1;
    put(out, row, 0, &format!(" Failure causes ({})", snap.fail_categories.len()), Color::Red, w);
    row += 1;
    for fc in snap.fail_categories.iter().take(avail.saturating_sub(row - top) as usize) {
        put(out, row, 1, &format!("{:>4}  {:<28} {}", fc.count, fc.cat, fc.hint), Color::Grey, w);
        row += 1;
    }
}

fn render_archive(out: &mut Stdout, snap: &Snapshot, top: u16, avail: u16, w: u16) {
    let a = &snap.archive;
    put(out, top, 0, &format!(" {} repos mirrored on disk", a.total), Color::Cyan, w);
    let mut row = top + 1;
    if !a.active.is_empty() {
        put(out, row, 0, &format!("  {} cloning now", a.active.len()), Color::Yellow, w);
        row += 1;
    }
    if a.pending_total > 0 {
        put(out, row, 0, &format!("  {} queued", a.pending_total), Color::Cyan, w);
        row += 1;
    }
    for r in a.recent.iter().take(avail.saturating_sub(row - top) as usize) {
        let color = if r.status == "failed" { Color::Red } else { Color::Green };
        put(out, row, 1, &format!("{} {} {}", if r.status == "failed" { "✗" } else { "+" }, r.repo, r.reason), color, w);
        row += 1;
    }
}

fn render_config(out: &mut Stdout, snap: &Snapshot, sel: usize, top: u16, w: u16) {
    let mut row = top;
    put(out, row, 0, " Configuration — ↑/↓ pick a field · ←/→ change it (applied live)", Color::Cyan, w);
    row += 2;
    let cf = &snap.config;
    let val = |k: &str| -> String {
        match cf.get(k) {
            Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
            Some(v) if v.is_boolean() => if v.as_bool().unwrap_or(false) { "[x] yes".into() } else { "[ ] no".into() },
            Some(v) if v.is_f64() => format!("{}", v.as_f64().unwrap_or(0.0)),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    };
    // editable live fields first (selected one highlighted with ‹ › / ▸)
    for (i, f) in CONFIG_FIELDS.iter().enumerate() {
        let v = if *f == "jobs" { snap.jobs.to_string() }
                else if *f == "paused" { if snap.paused { "[x] yes".into() } else { "[ ] no".into() } }
                else { val(f) };
        let active = i == sel;
        let marker = if active { "▸ " } else { "  " };
        let shown = if active { format!("‹ {} ›", v) } else { v };
        let color = if active { Color::Yellow } else { Color::Grey };
        put(out, row, 0, &format!("{}{:<14} {}", marker, f, shown), color, w);
        row += 1;
    }
    row += 1;
    // the rest of the config (read-only paths/source), for context
    put(out, row, 0, " (paths — restart to change)", Color::DarkGrey, w);
    row += 1;
    for k in ["source", "build_dir", "archive", "manage_venvs"] {
        if cf.contains_key(k) {
            put(out, row, 1, &format!("{:<14} {}", k, val(k)), Color::DarkGrey, w);
            row += 1;
        }
    }
    if !snap.dep_relaxations.is_empty() {
        row += 1;
        put(out, row, 0, " Dependency relaxations / overrides", Color::Cyan, w);
        row += 1;
        for l in snap.dep_relaxations.iter().take(4) {
            put(out, row, 1, l, Color::Grey, w);
            row += 1;
        }
    }
    if !snap.control_log.is_empty() {
        row += 1;
        put(out, row, 0, " Recent live changes", Color::Cyan, w);
        row += 1;
        for l in snap.control_log.iter().rev().take(4) {
            put(out, row, 1, l, Color::Grey, w);
            row += 1;
        }
    }
}

fn status_panel(snap: &Snapshot, ui: &Ui) -> String {
    match TABS[ui.tab] {
        "failures" => snap
            .failures_recent
            .get(ui.sel)
            .map(|f| format!(" {}  FAILED [{}]: {}", f.slug, prov_str(&f.compiler_version, &f.backend, &f.builder_version), f.error))
            .unwrap_or_else(|| " no failures".into()),
        "built" => snap
            .built_recent
            .get(ui.sel)
            .map(|b| format!(" {} ✓ built with {} — {}", b.slug, prov_str(&b.compiler_version, &b.backend, &b.builder_version), human(b.bytes)))
            .unwrap_or_else(|| " nothing built yet".into()),
        _ => {
            if snap.done {
                format!(
                    " done — built {} · failed {} · skipped {} of {}",
                    snap.counts.built, snap.counts.failed, snap.counts.skipped, snap.total
                )
            } else {
                format!(" building — {} queued, {} in flight", snap.counts.queued, snap.counts.building)
            }
        }
    }
}

fn render_detail(out: &mut Stdout, snap: &Snapshot, ui: &Ui, w: u16, h: u16) {
    let lines: Vec<String> = match TABS[ui.tab] {
        "failures" => snap
            .failures_recent
            .get(ui.sel)
            .map(|f| {
                vec![
                    format!("FAILURE: {}", f.slug),
                    format!("provenance: {}", prov_str(&f.compiler_version, &f.backend, &f.builder_version)),
                    format!("log: {}", f.log),
                    String::new(),
                    f.error.clone(),
                ]
            })
            .unwrap_or_default(),
        "built" => snap
            .built_recent
            .get(ui.sel)
            .map(|b| {
                vec![
                    format!("BUILT: {}", b.slug),
                    format!("provenance: {}", prov_str(&b.compiler_version, &b.backend, &b.builder_version)),
                    format!("size: {}   vs shipped: {}", human(b.bytes), if b.compare.is_empty() { "n/a" } else { &b.compare }),
                    format!("log: {}", b.log),
                ]
            })
            .unwrap_or_default(),
        _ => vec!["(no detail for this item)".into()],
    };
    let bh = (lines.len() as u16 + 4).min(h.saturating_sub(4));
    let bw = w.saturating_sub(6);
    let top = (h - bh) / 2;
    for r in 0..bh {
        put(out, top + r, 2, &" ".repeat(bw as usize), Color::Black, w);
    }
    put(out, top, 2, &format!("┌{}┐", "─".repeat(bw as usize - 2)), Color::Cyan, w);
    for (i, l) in lines.iter().enumerate() {
        put(out, top + 1 + i as u16, 4, l, Color::White, w);
    }
    put(out, top + bh - 1, 2, &format!("└{}┘  [Esc] close", "─".repeat(bw as usize - 2)), Color::Cyan, w);
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
