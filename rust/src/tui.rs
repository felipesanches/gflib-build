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
    sel_key: Option<String>, // the SELECTED item's identity (stable selection: cursor follows it)
}

/// The identity (slug/key) of each item in the current tab's list, in display order — used for
/// stable selection (the cursor tracks the item, not the row index, as lists reorder live).
fn list_keys(snap: &Snapshot, tab: usize) -> Vec<String> {
    match TABS[tab] {
        "config" => CONFIG_FIELDS.iter().map(|s| s.to_string()).collect(),
        "queue" => snap.queued_list.iter().map(|q| q.slug.clone()).collect(),
        "cohorts" => snap.cohorts.iter().map(|c| c.key.clone()).collect(),
        "built" => snap.built_recent.iter().map(|b| b.slug.clone()).collect(),
        "failures" => snap.failures_recent.iter().map(|f| f.slug.clone()).collect(),
        "stats" => snap.fail_categories.iter().map(|c| c.cat.clone()).collect(),
        "archive" => snap.archive.pending.clone(),
        _ => snap.building.iter().map(|b| b.slug.clone()).collect(),
    }
}

pub fn run(source: Arc<dyn Source>) -> std::io::Result<TuiResult> {
    let mut out = std::io::stdout();
    terminal::enable_raw_mode()?;
    queue!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    out.flush()?;

    let mut ui = Ui { tab: 1, sel: 0, detail: false, sel_key: None };
    let res = loop {
        let snap = source.snapshot();
        // stable selection: re-resolve the row index from the remembered item key each frame, so a
        // list reordering (failed→building→built) keeps the cursor on the SAME item.
        let keys = list_keys(&snap, ui.tab);
        if let Some(k) = &ui.sel_key {
            if let Some(i) = keys.iter().position(|x| x == k) {
                ui.sel = i;
            }
        }
        if ui.sel >= keys.len() {
            ui.sel = keys.len().saturating_sub(1);
        }
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
                        ui.sel_key = None;
                        ui.detail = false;
                    }
                    KeyCode::BackTab => {
                        ui.tab = (ui.tab + TABS.len() - 1) % TABS.len();
                        ui.sel = 0;
                        ui.sel_key = None;
                        ui.detail = false;
                    }
                    KeyCode::Up => {
                        if ui.sel > 0 {
                            ui.sel -= 1;
                        }
                        ui.sel_key = list_keys(&snap, ui.tab).get(ui.sel).cloned();
                    }
                    KeyCode::Down => {
                        ui.sel = (ui.sel + 1).min(list_len(&snap, ui.tab).saturating_sub(1));
                        ui.sel_key = list_keys(&snap, ui.tab).get(ui.sel).cloned();
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
    // completion banner (R5): when the build is done / the daemon died, say so prominently instead
    // of leaving the dashboard looking frozen mid-flight.
    if snap.done && snap.total > 0 {
        let top = snap.fail_categories.first().map(|f| format!("  ·  top cause: {} ({})", f.cat, f.count)).unwrap_or_default();
        put(out, 3, 0, &format!(" ✓ BUILD COMPLETE — built {} · failed {} · skipped {} of {}{}",
            c.built, c.failed, c.skipped, snap.total, top), Color::Green, w);
    } else if !src.is_live() && !snap.daemon_alive && snap.total > 0 {
        put(out, 3, 0, &format!(" ■ DAEMON STOPPED — built {} · failed {} · queued {} (re-run to resume)",
            c.built, c.failed, c.queued), Color::Yellow, w);
    } else {
        let barw = w.saturating_sub(8) as usize;
        let fill = barw * pct / 100;
        let bar: String = std::iter::repeat('#').take(fill).chain(std::iter::repeat('-').take(barw - fill)).collect();
        put(out, 3, 0, &format!(" [{}] {:>3}%", bar, pct), Color::Cyan, w);
    }

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

fn render_overview(out: &mut Stdout, snap: &Snapshot, top: u16, avail: u16, w: u16) {
    // water-fill the two sections so they fill the available height (header + items each)
    let desired = [snap.building.len() + 1, snap.failures_recent.len() + 1];
    let alloc = water_fill(&desired, avail as usize);
    let mut row = top;
    put(out, row, 0, &format!(" Now building ({})", snap.building.len()), Color::DarkYellow, w);
    row += 1;
    for b in snap.building.iter().take(alloc[0].saturating_sub(1)) {
        put(out, row, 1, &format!("w{} {:<44} {:>6} {}", b.worker, b.slug, hms(b.dur), b.note), Color::Grey, w);
        row += 1;
    }
    put(out, row, 0, &format!(" Recent failures ({})", snap.failures_recent.len()), Color::Red, w);
    row += 1;
    for f in snap.failures_recent.iter().take(alloc[1].saturating_sub(1)) {
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
    if !snap.op_stats.is_empty() {
        row += 1;
        put(out, row, 0, " Per-operation timing (total · count · mean · max)", Color::Cyan, w);
        row += 1;
        let mut ops: Vec<_> = snap.op_stats.iter().collect();
        ops.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap_or(std::cmp::Ordering::Equal));
        for (op, s) in ops.iter().take(7) {
            put(out, row, 1, &format!("{:<10} {:>8.1}s  n={:<5} mean {:.2}s  max {:.1}s",
                op, s.total, s.count, s.mean, s.max), Color::Grey, w);
            row += 1;
        }
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
    let unreachable = a.recent.iter().filter(|r| r.status == "failed").count();
    put(out, top, 0, &format!(" {} repos mirrored on disk", a.total), Color::Cyan, w);
    put(out, top + 1, 0, &format!("  {} cloning now   {} queued   {} unreachable",
        a.active.len(), a.pending_total, unreachable), Color::Grey, w);
    let mut row = top + 3;

    // multi-column grid: cloning-now (yellow) · recently-archived (green) · queued-next (cyan), to
    // fit as many repo slugs on screen as possible. Each section is a labelled colour block.
    let colw: usize = 30;
    let cols = (w as usize / colw).max(1);
    let grid = |out: &mut Stdout, row: &mut u16, items: &[String], color: Color, label: &str, maxrows: u16| {
        if items.is_empty() || maxrows == 0 { return; }
        put(out, *row, 0, label, color, w);
        *row += 1;
        let mut shown = 0;
        let cap = (maxrows as usize - 1).saturating_mul(cols);
        for chunk in items.iter().take(cap).collect::<Vec<_>>().chunks(cols) {
            let line: String = chunk.iter().map(|s| format!("{:<width$}", trunc(s, colw - 1), width = colw)).collect();
            put(out, *row, 1, &line, color, w);
            *row += 1;
            shown += chunk.len();
        }
        if items.len() > shown {
            put(out, *row, 1, &format!("… +{} more", items.len() - shown), Color::DarkGrey, w);
            *row += 1;
        }
        *row += 1;
    };
    let budget = avail.saturating_sub(3);
    let recent_added: Vec<String> = a.recent.iter().filter(|r| r.status != "failed").map(|r| r.repo.clone()).collect();
    let unreach: Vec<String> = a.recent.iter().filter(|r| r.status == "failed")
        .map(|r| format!("{} ({})", r.repo, trunc(&r.reason, 16))).collect();
    grid(out, &mut row, &a.active, Color::Yellow, " cloning now", budget / 4);
    grid(out, &mut row, &recent_added, Color::Green, " recently archived (last 30 min)", budget / 3);
    grid(out, &mut row, &a.pending, Color::Cyan, " queued next", budget / 3);
    grid(out, &mut row, &unreach, Color::Red, " unreachable (git reason)", budget / 4);
    if a.active.is_empty() && a.recent.is_empty() && a.pending.is_empty() {
        put(out, row, 1, "(archive idle — nothing being mirrored)", Color::DarkGrey, w);
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n.saturating_sub(1)).collect::<String>() + "…" }
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
