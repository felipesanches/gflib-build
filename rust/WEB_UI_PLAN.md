# Web UI — analysis & implementation plan

The web dashboard (`--ui web`, `src/web.rs`) is an early, dependency-free HTTP/1.1 server
(std `TcpListener`, one thread/connection) that serves a single HTML page polling
`/api/status` every 1.5 s and posting live controls to `/api/control`. It works, but it has
drifted well behind the curses TUI.

**Guiding principles**

1. **Mirror the TUI's structure.** A user who switches between the terminal and the browser
   should not have to re-learn anything: the **tab order and the section order/content must match
   the TUI** (`config · overview · queue · cohorts · archive · built · failures · stats`, and the
   same sections within each tab). The TUI is the source of truth for *what* is shown and *in what
   order*; the web may render it more richly.
2. **Do the things a terminal can't.** Charts, sortable/filterable tables, live log streaming,
   form controls, multi-pane layouts, deep links, notifications.
3. **Stay dependency-free.** No npm, no CDN libraries (offline-safe, supply-chain-clean — the
   whole point of the Rust port). Charts are hand-rolled **inline SVG** in vanilla JS.
4. **No new daemon coupling unless needed.** Almost everything is derivable from the existing
   `/api/status` snapshot. Only a couple of enhancements need a new endpoint.

---

## Part A — bring the web UI to TUI parity (structure)

These close the gap between the current page and the TUI. They are prerequisites for everything
else and are mostly mechanical ports of work already done in `tui.rs`.

| # | Item | Current web | Target (match TUI) |
|---|------|-------------|--------------------|
| A1 | **Tab order** | `overview,queue,cohorts,built,failures,stats,archive,config` | `config,overview,queue,cohorts,archive,built,failures,stats` (the `VIEWS` order) |
| A2 | **Header** | "Google Fonts library build — Rust port" | " Google Fonts library build" (+ `[PAUSED]`), add `cohorts N`, elapsed right-aligned |
| A3 | **Progress bar** | single green fill; denominator includes `skipped` | **segmented** built=green / failed=red / not-yet-attempted=dim; denominator = in-scope (built+failed+queued+building, **excludes skipped**); centred `done/inscope attempted (pct%) · K skipped` label; archive/cohorts **phase-bar** mode |
| A4 | **Sections** | each tab = one flat table | the same sections the TUI builds (`sections_for`): overview = Pipeline + Recent failures; failures = by-cause + newest + history; stats = migration summary + Phase timing + Operation timing |
| A5 | **Cohorts** | `key + count` only | list **family display names** (now carried in the snapshot) joined by ` | `, with the cached/uncached dot |
| A6 | **Archive** | only `recent` | the pre-warmer view: **cloning-now / queued / recently-added / unreachable** (`archive.active/pending/recent`) + total mirrored |
| A7 | **Config** | read-only key/value dump | the full 14-field schema with current values + tags; (editing → Part C) |
| A8 | **Controls** | `pause/resume/jobs±` (jobs± is fabricated) | match TUI live controls: pause/resume, retry-selected, and the config "apply" (no ad-hoc jobs± buttons) |
| A9 | **Colours/provenance** | partial | same column colours + `compiler · builder` provenance as the TUI lists |

**Refactor note:** as the page grows, split the monolithic `PAGE` constant into a few `const`
strings (head/style, body, script) or serve a couple of static assets from `web.rs`. Keep the
zero-dependency, single-binary property (assets compiled in via `include_str!`).

---

## Part B — web-only: charts (inline SVG, derived from the snapshot, **no backend change**)

All of these are computable from the existing `/api/status` snapshot. Render as inline SVG with a
small vanilla-JS helper (`bars()`, `donut()`, `ring()`, `spark()`); ~150 lines total, no deps.

- **B1 — Failure-cause bar chart.** Horizontal bars from `fail_categories` (count per cause),
  colour-graded, click-through to filter the failures table. The single most useful "where are we
  losing builds" view.
- **B2 — Per-operation timing bars (bottlenecks).** Horizontal bars from `op_stats` (total seconds
  per op: clone / extract / venv / build / compare). Shows where wall-clock goes.
- **B3 — Backend mix donut.** `migration` → fontc / fontmake-fallback / fontmake-only (and
  both-identical/both-differ). The fontc-migration story at a glance (M-milestones).
- **B4 — Cohort-size bars.** Top-N cohorts by family count, cached vs not coloured. Shows which
  dependency sets dominate the venv cost.
- **B5 — Archive-mirroring ring.** A progress ring: mirrored / (mirrored + pending) with the live
  clone rate; complements Part A6.
- **B6 — Outcome donut.** built / failed / queued / building / skipped — the same numbers as the
  progress bar, as a donut for the overview.

Placement: an enriched **overview** (charts + now-building + recent failures, using the browser's
extra width in a responsive grid), plus the relevant chart embedded in its own tab (B1/history on
*failures*, B2/B3 on *stats*, B4 on *cohorts*, B5 on *archive*).

---

## Part C — web-only: interactivity

- **C1 — Per-family detail panel.** Click any row (building/built/failed/queued/cohort) → a side
  panel with the same content as the TUI detail overlay (`build_detail`), incl. the **log tail**.
  Needs a small new endpoint **`GET /api/log?slug=…&n=200`** that returns the tail of
  `build_dir/logs/<slug>.log` (read-only). This is the one genuinely new backend route.
- **C2 — Live log streaming.** Extend C1 to poll the log tail while a family is *building* (or use
  chunked transfer / SSE later). Watch a compile in real time in the browser.
- **C3 — Config form editor.** Render the schema (Part A7) as real form controls — sliders for
  `jobs`/`percent`, a `<select>` for `backend`, checkboxes for the bools — and POST changed
  *live* fields to `/api/control` (the existing channel). Friendlier than the TUI's text editing.
- **C4 — Sortable / filterable tables.** Click a column to sort; a search box to filter
  built/failed/queue by slug or cause. Pure client-side over the snapshot arrays.
- **C5 — Deep links.** Reflect the active tab in `location.hash` (`#stats`) so a view is
  bookmarkable/shareable; restore on load.
- **C6 — Export.** "Download snapshot JSON" and "Download built/failed as CSV" buttons (client-side
  Blob; no backend).

---

## Part D — web-only: timeseries (live history)

Charts that need *change over time*, not just the current snapshot.

- **D1 — Client-side accumulation (MVP).** The page already polls every 1.5 s; keep a bounded ring
  buffer of `{t, built, failed, queued, building, disk}` samples in JS and draw:
  - **Build progress** line chart (cumulative built/failed over elapsed),
  - **Throughput** sparkline (Δbuilt per minute) in the header,
  - **Disk growth** line (build + archive).
  Zero backend change; resets on reload (acceptable for a live monitor).
- **D2 — Daemon-side history (later).** For history that survives a reload, add a small rolling
  series to the snapshot (or a `GET /api/history` that downsamples `events.jsonl`). Optional;
  only if D1's reload-reset proves annoying.

---

## Part E — polish

- **E1 — Responsive multi-pane layout** (CSS grid) — the browser has far more room than a terminal;
  show overview charts + now-building + recent failures together instead of one tab at a time.
- **E2 — Pause polling when the tab is hidden** (`document.visibilitychange`) — don't hammer the
  daemon in a background tab.
- **E3 — Completion notification** (Notification API) when the build finishes or hits a milestone.
- **E4 — Theme** (dark default; a light toggle) and a manual refresh / refresh-interval control.

---

## Suggested sequencing

1. **W1 = Part A** ✅ **DONE (2026-06-05, commit a4a17d1).** The web page now matches the TUI's tab
   order, segmented progress bar, sections, cohort family names, archive pre-warmer grid, full config
   schema, and real controls (no fabricated jobs±). Verified by a spec-extraction workflow + an
   adversarial review workflow (12 divergences found & fixed). The `PAGE`-splitting refactor was
   deferred (the page is still one `const` — split it when W2's charts grow it).
2. **W2 = Part B** ✅ **DONE (2026-06-05, commit d78d9ef).** Inline-SVG donuts/rings + CSS bars,
   all snapshot-derived (no backend change): overview outcome donut + top-causes bars; failures
   by-cause bars; stats op-timing bars + backend-mix donut; cohort-size bars; archive mirroring ring.
   Verified by an adversarial chart-review workflow (0 confirmed bugs).
3. **W3 = Part C1–C3** ✅ **DONE (2026-06-05, commits b9dfb0a + 4310a3e).** Click-to-detail panel for
   every row (incl. cohorts, mirroring the TUI build_detail); the new `GET /api/log` endpoint
   (traversal-safe) feeds the log tail; the config form edits the live keys (backend/jobs/percent/
   compare) via `/api/control`. Also: cohort family names are now coloured by build status in BOTH
   UIs. Verified by an adversarial review workflow (0 confirmed bugs).
4. **W4 = Part D1** (client-side timeseries) and **Part C4–C6** (tables/links/export).
5. **W5 = Part E** (polish) and, if wanted, **Part D2** (daemon history).

**Out of scope by design:** the first-run **setup wizard** stays TUI/CLI-only (the web UI is a
monitor + live editor, not a launcher); pixel-identical parity with the *old Python* web UI (we are
free to diverge and improve, only the tab/section *ordering* must match the TUI).

**Testing:** drive the page headlessly the same way the TUI is verified — fetch `/api/status` and
assert the rendered structure — plus a couple of `curl` checks for `/api/control` and the new
`/api/log`. No browser automation needed for the data layer.
