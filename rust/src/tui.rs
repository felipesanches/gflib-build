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
    detail_lines: Vec<String>, // detail content captured ONCE when the overlay opens (no per-frame I/O)
    dscroll: usize,          // scroll offset within the detail overlay
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

    let mut ui = Ui { tab: 1, sel: 0, detail: false, sel_key: None, detail_lines: Vec::new(), dscroll: 0 };
    let mut prev: Option<Screen> = None;
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
        // draw the whole frame into a back-buffer, then emit only the cells that changed (no flicker)
        let (w, h) = terminal::size().unwrap_or((100, 40));
        let mut scr = Screen::new(w.max(1), h.max(1));
        render(&mut scr, &snap, &ui, &*source);
        flush_diff(&mut out, &scr, &prev)?;
        prev = Some(scr);

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
                        if ui.detail {
                            ui.dscroll = ui.dscroll.saturating_sub(1); // scroll the overlay
                        } else {
                            if ui.sel > 0 {
                                ui.sel -= 1;
                            }
                            ui.sel_key = list_keys(&snap, ui.tab).get(ui.sel).cloned();
                        }
                    }
                    KeyCode::Down => {
                        if ui.detail {
                            ui.dscroll = (ui.dscroll + 1).min(ui.detail_lines.len().saturating_sub(1));
                        } else {
                            ui.sel = (ui.sel + 1).min(list_len(&snap, ui.tab).saturating_sub(1));
                            ui.sel_key = list_keys(&snap, ui.tab).get(ui.sel).cloned();
                        }
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
                    KeyCode::Enter => {
                        if ui.detail {
                            ui.detail = false;
                        } else {
                            // capture the detail ONCE (reads the log-file tail here, not every frame)
                            ui.detail_lines = build_detail(&snap, ui.tab, ui.sel, &source.build_dir());
                            ui.dscroll = 0;
                            ui.detail = true;
                        }
                    }
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

// ---- flicker-free rendering: draw the whole frame into a back-buffer of cells, then emit ONLY the
// cells that changed since the previous frame (like ncurses does). No full-screen Clear → no flicker.
#[derive(Clone, PartialEq)]
struct Cell {
    ch: char,
    fg: Color,
}
impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Reset }
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
    fn set(&mut self, row: u16, col: u16, text: &str, fg: Color) {
        if row >= self.h {
            return;
        }
        let mut c = col;
        for ch in text.chars().filter(|c| !c.is_control()) {
            if c >= self.w {
                break;
            }
            let idx = (row as usize) * (self.w as usize) + c as usize;
            self.cells[idx] = Cell { ch, fg };
            c += 1;
        }
    }
}

/// Draw `text` at (row,col) into the back-buffer `scr`. (`_w` kept for call-site compatibility.)
fn put(scr: &mut Screen, row: u16, col: u16, text: &str, color: Color, _w: u16) {
    scr.set(row, col, text, color);
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
            let start = col;
            let mut run = String::new();
            while col < new.w {
                let i = (row as usize) * (new.w as usize) + col as usize;
                let cc = &new.cells[i];
                let ch_changed = full || prev.as_ref().map(|p| p.cells.get(i) != Some(cc)).unwrap_or(true);
                if !ch_changed || cc.fg != fg {
                    break;
                }
                run.push(cc.ch);
                col += 1;
            }
            queue!(out, cursor::MoveTo(start, row), SetForegroundColor(fg), Print(run), ResetColor)?;
        }
    }
    out.flush()
}

fn render(scr: &mut Screen, snap: &Snapshot, ui: &Ui, src: &dyn Source) {
    let (w, h) = (scr.w, scr.h);

    // ---- header ----
    let mode = if src.is_live() { "live" } else if snap.daemon_alive { "monitor" } else { "stopped" };
    let title = format!(
        " Google Fonts library build — Rust port [{}]{}",
        mode,
        if snap.paused { "  [PAUSED]" } else { "" }
    );
    put(scr, 0, 0, &title, Color::White, w);
    put(scr, 0, w.saturating_sub(18), &format!("elapsed {}", hms(snap.elapsed)), Color::Grey, w);

    let bld = snap.disk_build_total;
    let arc = snap.disk_archive_total;
    // Always spell out both components — no ambiguous "(build dir)".
    let disk = if snap.disk_archive_nested {
        format!("disk used {} (build + nested archive, all included)", human(bld))
    } else {
        format!("disk used {} (build {} + archive {})", human(bld + arc), human(bld), human(arc))
    };
    put(
        scr,
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
        scr,
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
        put(scr, 3, 0, &format!(" ✓ BUILD COMPLETE — built {} · failed {} · skipped {} of {}{}",
            c.built, c.failed, c.skipped, snap.total, top), Color::Green, w);
    } else if !src.is_live() && !snap.daemon_alive && snap.total > 0 {
        put(scr, 3, 0, &format!(" ■ DAEMON STOPPED — built {} · failed {} · queued {} (re-run to resume)",
            c.built, c.failed, c.queued), Color::Yellow, w);
    } else {
        let barw = w.saturating_sub(8) as usize;
        let fill = barw * pct / 100;
        let bar: String = std::iter::repeat('#').take(fill).chain(std::iter::repeat('-').take(barw - fill)).collect();
        put(scr, 3, 0, &format!(" [{}] {:>3}%", bar, pct), Color::Cyan, w);
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
    put(scr, 4, 0, &tabline, Color::Yellow, w);

    // ---- pinned now-building (on every tab) ----
    let body_top = 6u16;
    let footer_row = h.saturating_sub(2);
    let panel_row = h.saturating_sub(3);
    let mut row = body_top;
    if !snap.building.is_empty() && TABS[ui.tab] != "overview" {
        put(scr, row, 0, &format!(" Now building ({})", snap.building.len()), Color::DarkYellow, w);
        row += 1;
        for b in snap.building.iter().take(3) {
            put(
                scr,
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
        "overview" => render_overview(scr, snap, row, avail, w),
        "queue" => render_list_simple(
            scr, row, avail, w, ui.sel, "Queue",
            snap.queued_list.iter().map(|q| format!("{:<48} {}", q.slug, q.kind)).collect(),
        ),
        "cohorts" => render_list_simple(
            scr, row, avail, w, ui.sel, "Cohorts",
            snap.cohorts.iter().map(|c| format!("{:<20} {} families", c.key, c.count)).collect(),
        ),
        "built" => render_built(scr, snap, row, avail, w, ui.sel),
        "failures" => render_failures(scr, snap, row, avail, w, ui.sel),
        "stats" => render_stats(scr, snap, row, avail, w),
        "archive" => render_archive(scr, snap, row, avail, w),
        _ => render_config(scr, snap, ui.sel, row, w),
    }

    // ---- status panel ----
    put(scr, panel_row, 0, &"─".repeat(w as usize), Color::DarkGrey, w);
    put(scr, panel_row, 0, &status_panel(snap, ui), Color::White, w);

    // ---- footer ----
    put(scr, footer_row, 0,
        " [Tab/⇧Tab]tabs  [↑↓]item  [↵]details  [p]ause  [R]etry  [+/-]jobs  [C]onfig  [q]uit",
        Color::DarkGrey, w);

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

fn render_overview(scr: &mut Screen, snap: &Snapshot, top: u16, avail: u16, w: u16) {
    // water-fill the two sections so they fill the available height (header + items each)
    let desired = [snap.building.len() + 1, snap.failures_recent.len() + 1];
    let alloc = water_fill(&desired, avail as usize);
    let mut row = top;
    put(scr, row, 0, &format!(" Now building ({})", snap.building.len()), Color::DarkYellow, w);
    row += 1;
    for b in snap.building.iter().take(alloc[0].saturating_sub(1)) {
        put(scr, row, 1, &format!("w{} {:<44} {:>6} {}", b.worker, b.slug, hms(b.dur), b.note), Color::Grey, w);
        row += 1;
    }
    put(scr, row, 0, &format!(" Recent failures ({})", snap.failures_recent.len()), Color::Red, w);
    row += 1;
    for f in snap.failures_recent.iter().take(alloc[1].saturating_sub(1)) {
        put(scr, row, 1, &format!("{:<36} {}", f.slug, f.error), Color::Grey, w);
        row += 1;
    }
}

fn render_list_simple(
    scr: &mut Screen,
    top: u16,
    avail: u16,
    w: u16,
    sel: usize,
    title: &str,
    items: Vec<String>,
) {
    put(scr, top, 0, &format!(" {} ({})", title, items.len()), Color::Cyan, w);
    let mut row = top + 1;
    let start = scroll_start(sel, avail as usize - 1, items.len());
    for (i, it) in items.iter().enumerate().skip(start).take(avail as usize - 1) {
        let color = if i == sel { Color::Yellow } else { Color::Grey };
        let marker = if i == sel { "▸ " } else { "  " };
        put(scr, row, 0, &format!("{}{}", marker, it), color, w);
        row += 1;
    }
}

fn render_built(scr: &mut Screen, snap: &Snapshot, top: u16, avail: u16, w: u16, sel: usize) {
    put(
        scr,
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
            scr,
            row,
            0,
            &format!("{}{:<30} {:<34} {}", marker, b.slug, prov, human(b.bytes)),
            color,
            w,
        );
        row += 1;
    }
}

fn render_failures(scr: &mut Screen, snap: &Snapshot, top: u16, avail: u16, w: u16, sel: usize) {
    put(scr, top, 0, &format!(" Failures ({})", snap.failures_recent.len()), Color::Red, w);
    let mut row = top + 1;
    let start = scroll_start(sel, avail as usize - 1, snap.failures_recent.len());
    for (i, f) in snap.failures_recent.iter().enumerate().skip(start).take(avail as usize - 1) {
        let color = if i == sel { Color::Yellow } else { Color::Grey };
        let marker = if i == sel { "▸ " } else { "  " };
        put(scr, row, 0, &format!("{}{:<32} {}", marker, f.slug, f.error), color, w);
        row += 1;
    }
}

fn render_stats(scr: &mut Screen, snap: &Snapshot, top: u16, avail: u16, w: u16) {
    let mut row = top;
    put(scr, row, 0, " Migration", Color::Green, w);
    row += 1;
    for (k, v) in &snap.migration {
        put(scr, row, 1, &format!("{:<22} {}", k, v), Color::Grey, w);
        row += 1;
    }
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
    if !snap.op_stats.is_empty() {
        row += 1;
        put(scr, row, 0, " Per-operation timing (total · count · mean · max)", Color::Cyan, w);
        row += 1;
        let mut ops: Vec<_> = snap.op_stats.iter().collect();
        ops.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap_or(std::cmp::Ordering::Equal));
        for (op, s) in ops.iter().take(7) {
            put(scr, row, 1, &format!("{:<10} {:>8.1}s  n={:<5} mean {:.2}s  max {:.1}s",
                op, s.total, s.count, s.mean, s.max), Color::Grey, w);
            row += 1;
        }
    }
    row += 1;
    put(scr, row, 0, &format!(" Failure causes ({})", snap.fail_categories.len()), Color::Red, w);
    row += 1;
    for fc in snap.fail_categories.iter().take(avail.saturating_sub(row - top) as usize) {
        put(scr, row, 1, &format!("{:>4}  {:<28} {}", fc.count, fc.cat, fc.hint), Color::Grey, w);
        row += 1;
    }
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

fn render_config(scr: &mut Screen, snap: &Snapshot, sel: usize, top: u16, w: u16) {
    let mut row = top;
    put(scr, row, 0, " Configuration — ↑/↓ pick a field · ←/→ change it (applied live)", Color::Cyan, w);
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
        put(scr, row, 0, &format!("{}{:<14} {}", marker, f, shown), color, w);
        row += 1;
    }
    row += 1;
    // the rest of the config (read-only paths/source), for context
    put(scr, row, 0, " (paths — restart to change)", Color::DarkGrey, w);
    row += 1;
    for k in ["source", "build_dir", "archive", "manage_venvs"] {
        if cf.contains_key(k) {
            put(scr, row, 1, &format!("{:<14} {}", k, val(k)), Color::DarkGrey, w);
            row += 1;
        }
    }
    if !snap.dep_relaxations.is_empty() {
        row += 1;
        put(scr, row, 0, " Dependency relaxations / overrides", Color::Cyan, w);
        row += 1;
        for l in snap.dep_relaxations.iter().take(4) {
            put(scr, row, 1, l, Color::Grey, w);
            row += 1;
        }
    }
    if !snap.control_log.is_empty() {
        row += 1;
        put(scr, row, 0, " Recent live changes", Color::Cyan, w);
        row += 1;
        for l in snap.control_log.iter().rev().take(4) {
            put(scr, row, 1, l, Color::Grey, w);
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
fn build_detail(snap: &Snapshot, tab: usize, sel: usize, build_dir: &std::path::Path) -> Vec<String> {
    let mut o: Vec<String> = Vec::new();
    let logname = |slug: &str| build_dir.join("logs").join(format!("{}.log", slug.replace('/', "__")));
    match TABS[tab] {
        "failures" => {
            if let Some(f) = snap.failures_recent.get(sel) {
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
            }
        }
        "built" => {
            if let Some(b) = snap.built_recent.get(sel) {
                o.push(format!("Built: {}", b.slug));
                o.push(format!("backend: {}", if b.backend.is_empty() { "?" } else { &b.backend }));
                o.push(format!("output size: {}", human(b.bytes)));
                o.push(format!("vs shipped: {}", if b.compare.is_empty() { "(not compared)" } else { &b.compare }));
                o.push(format!("provenance: {}", prov_str(&b.compiler_version, &b.backend, &b.builder_version)));
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
                o.push(String::new());
                o.push("family names:".into());
                if c.families.is_empty() {
                    o.push("  (none assigned yet)".into());
                }
                for n in &c.families {
                    o.push(format!("  {}", n));
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
        "stats" => {
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
        "overview" => {
            if let Some(b) = snap.building.get(sel) {
                let logp = logname(&b.slug);
                o.push(format!("Building: {}", b.slug));
                o.push(format!("worker: {}", b.worker));
                o.push(format!("elapsed: {}", hms(b.dur)));
                o.push(format!("step: {}", if !b.note.is_empty() { &b.note } else if !b.backend.is_empty() { &b.backend } else { "(starting)" }));
                o.push(format!("log: {}", logp.display()));
                o.push("log tail:".into());
                for ln in read_log_tail(&logp.to_string_lossy(), 60) {
                    o.push(format!("  {}", ln));
                }
            }
        }
        "archive" => {
            if let Some(repo) = snap.archive.pending.get(sel) {
                o.push(format!("Queued to mirror: {}", repo));
                o.push(String::new());
                o.push("This upstream repo is not yet in the archive; it will be cloned (append-only) when --mirror-missing is on.".into());
            }
        }
        "config" => {
            let f = CONFIG_FIELDS.get(sel).copied().unwrap_or("");
            o.push(format!("Setting: {}", f));
            o.push(String::new());
            o.push(match f {
                "jobs" => "Number of parallel build workers. ←/→ to change; applied live to the running build.",
                "percent" => "Build an evenly-spaced P% sample of the library. Raising it live enqueues the newly-included families.",
                "backend" => "Compiler: auto (fontc-first, fontmake fallback) · fontc · fontmake · both (build with each and compare).",
                "compare" => "sha256-compare the built fonts to the shipped binaries (metadata mode only).",
                "paused" => "Pause / resume the worker pool.",
                _ => "",
            }.to_string());
        }
        _ => o.push("(no detail for this item)".into()),
    }
    if o.is_empty() {
        o.push("(no detail for this item)".into());
    }
    o
}

fn render_detail(scr: &mut Screen, ui: &Ui, w: u16, h: u16) {
    // full-width overlay covering the body region (like the Python detail overlay), scrollable.
    let top = 5u16;
    let body_top = top + 1;
    let view = h.saturating_sub(body_top + 1).max(1) as usize; // leave the bottom row free
    let inner = (w.saturating_sub(3)).max(10) as usize;
    // word-wrap the captured logical lines; mark "header:" lines (no leading space + ends with ':')
    let mut wrapped: Vec<(String, bool)> = Vec::new();
    for l in &ui.detail_lines {
        if l.is_empty() {
            wrapped.push((String::new(), false));
            continue;
        }
        let is_hdr = !l.starts_with(' ') && l.ends_with(':');
        for wl in wrap_line(l, inner) {
            wrapped.push((wl, is_hdr));
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
    for (i, (wl, is_hdr)) in wrapped.iter().skip(ds).take(view).enumerate() {
        let color = if *is_hdr { Color::Cyan } else { Color::White };
        put(scr, body_top + i as u16, 1, wl, color, w);
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
