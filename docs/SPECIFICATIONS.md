# Specifications

These are the **original requirements** for `gflib-build`, recorded verbatim in intent so nothing is
lost as the tool evolves (this preserves requirement #7 below — *"save these specification
messages"* — now that the top-level [`README.md`](../README.md) is a concise overview rather than the
full spec). They describe the design the implementation realizes; some wording predates later
features and the move to a Rust implementation, but the intent is unchanged.

---

## Specifications (as given by Felipe)

This tool is built to the following requirements. They are recorded here verbatim in
intent so nothing is lost as the tool evolves.

1. **Run natively, on your own machine.** Produce the build *rules* so they can be executed
   independently on a machine with plenty of local storage.
2. **Live, interactive terminal UI** (ncurses or a similar terminal-app library)
   giving real-time feedback on:
   - **(A)** which families were already built,
   - **(B)** which ones are being built at a given moment,
   - **(C)** how much space was used so far,
   - **(D)** how long the build has been running,
   - **(E)** how many failures happened so far — **with detailed logs of the failures**.
3. **Clean any pre-built font project** — to save space and to guarantee we build
   everything **from scratch**.
4. The build must be based on the **pristine original state of the cloned (archived)
   repos**.
5. The build procedure **must not change the repos themselves**; instead, save all
   assets in a **separate build directory**.
6. **Never delete the archives.**
7. **Save these specification messages in this README.** (This section.)
8. **Keep the build system in its own git repository, and commit often.**
9. **Optimize total build time** by employing **parallelism**.
10. **Be smart about installing Python dependencies** when they are needed for build
    steps. If many families use the same set of Python dependencies, place them in a
    **cohort** so they can **share a single virtual environment**.
11. **Reduce reliance on Python.** If a family can be built with **Rust +
    gftools-builder3 (fontc)**, that is the success path. Families that still need
    **gftools-builder2 / Python** (e.g. for pre-build steps or Python-only deps) are
    fine as their own cohorts.
12. **Long-term goal: migrate everything to Rust.** This tool also *measures* how much
    of the library already builds with the Rust path, to track that migration.
13. **The terminal UI must be optional and modular.** Some users prefer a traditional
    terminal program; others may feed everything to a web interface. Build it modular so
    others can customize the frontend.
14. **Allow building only a percentage of the library** (e.g. 5%) instead of the whole
    thing — useful for validating the tool during development.
15. **Bootstrap from nothing.** With `--source metadata` (and by default, no flags), the
    tool should `git clone` google/fonts, read all `METADATA.pb`, **create/populate a local
    archive** of upstream repos (building one from scratch if none exists), generate the
    cohorts, and build them all — so any user can bootstrap the whole process. All of it
    shown live on an ncurses UI the user can observe and **navigate** as the data updates.
16. **Configuration screen.** It must **ask the user before** doing those heavy things
    (cloning, mirroring) — via the Configuration tab, not silent. (No separate "wizard": the
    first-run setup screen *is* the Configuration tab.)
17. **The Configuration screen is an editable ncurses form** with fields pre-populated with the default
    settings that the user can edit and then move on. Specifically: text fields render a
    movable cursor; a *build timeout* checkbox reveals a seconds field only when ticked
    (otherwise builds never time out — the user can still stop them in the UI); *percent of
    library* reacts to ←/→ in ±5 steps yet stays typeable for finer control; the *fontc*
    binary is auto-detected (offering to build it from source, or to type a path, if not
    found); and a pre-existing **repo archive is auto-detected** too.
18. **Persist the chosen settings** to a config file and pre-fill them on the next run.
19. **Auto-detect cargo**; if missing, give the user clear install instructions.
20. **Save all build log output** so failures can be read for troubleshooting.
21. **Time-measure every operation** to surface bottlenecks and guide performance work.
22. **Detach & reattach.** The user can quit the program and leave the builds running
    autonomously; reopening shows the live stats of what's going on.
23. **The entire interaction is ncurses.** The bootstrap operations (clone fontc, build
    fontc, clone google/fonts, populate archive, scan cohorts, build) all show up as a live
    **task-list with emoji status** (✅ done · 🔄 running · ⏳ pending · ❌ failed · ➖ n/a),
    plus per-task percentage and elapsed time — not plain CLI prints.
24. **Quit frees the shell; resume is instant.** Pressing `q` returns to the shell with the
    build still running; re-running the program (from the same or a different terminal)
    reattaches straight to live updates **without showing the setup interface again**.
25. **Live archive list.** As repos are populated into the archive, their names appear in a
    gradually growing list in the UI.
26. **Arrow-key tabs.** The dashboard's view tabs are driven by the ←/→ arrows (number keys
    still jump directly).
27. **Now-building shows checkouts.** A family appears in the "Now building" list (with a
    `checkout` tag) from the moment its source is being extracted, not only once it compiles.
28. **List selection + detail.** On every tab, `↑`/`↓` select an item in that tab's list and
    `↵` opens a detail overlay (a failure's full error + log tail, a cohort's requirements,
    etc.); `Esc`/`←`/`↵` returns.
29. **Cumulative clock.** The elapsed timer represents the real time spent so far across
    reopen/resume — it is not reset to zero when the program is reopened.
30. **`C` returns to the setup wizard** from the live dashboard (to change settings / start
    over); the running build is replaced only if the user actually starts a new one.
31. **Dynamic, streaming pipeline (no barriers).** Mirroring, cohort assignment, and building
    run concurrently: a repo is evaluated for its cohort and built the moment it's available
    in the archive — the build does not wait for "populate archive" to finish. An archive
    pre-warmer mirrors ahead using idle I/O; a shared per-repo clone lock means no repo is
    cloned twice; clones are abortable so shutdown never blocks. The archive list grows live.
32. **Configuration tab + LIVE config changes.** A "config" tab shows the live settings and
    edits them; applying takes effect on the RUNNING build with no restart — raising percent
    fetches/cohorts/builds the newly-included families, raising jobs starts more parallel
    workers (via a `control.json` the daemon polls). Drive config from one monitor at a time.
33. **Self-healing dependencies — no manual pin management.** If a venv's `pip install` can't
    satisfy a pinned version (a stale/dev pin absent from PyPI), the installer automatically
    drops just that pin, lets pip backtrack to a compatible version, retries, and records the
    relaxation in the config tab. Valid pins are kept, so reproducibility holds for everything
    that resolves; the user never has to hand-edit `requirements-build.txt` to unblock a build.
34. **Config tab is leftmost + the default view**, reflecting the current settings (falls back
    to the persisted config so it never shows a list of `None`).
35. **`--reset` completely cleans the system** — deletes all built assets + virtual environments
    (the whole build dir); the repo archive is NEVER touched (strict append-only policy).
36. **No separate wizard — the Configuration tab is the setup.** First-run setup is the
    Configuration tab itself (dashboard chrome, all fields editable, ▶ Start), and the same tab
    in the live dashboard does live edits. One config interface, not two.
37. **One Configuration tab, fully editable in place.** ←/→ move the text cursor / ±step /
    cycle, type to edit — no reloading a separate screen to edit. Tab navigation is `Tab` /
    `Shift-Tab` only (so ←/→ and number keys are free for editing).
38. **A "built" tab** lists the successfully built families (successes), parallel to Failures.
39. **Section navigation** — multi-section tabs (overview, stats) are navigable section by
    section: ←/→ focus a section (the ▼-marked one), ↑/↓ navigate its items, ↵ acts on the
    selected item. (Ctrl+Tab is intercepted by most terminals, so ←/→ is the binding.)
40. **Always-visible status panel.** A panel just above the footer instantly shows the most
    useful info about whatever item is focused, anywhere in the program — a single line in most
    cases (status-bar style), growing to a few lines when there's more worth showing. The
    motivating case: a red `✗` entry in *Overview → Archive — mirrored* now explains *why* that
    repo could not be mirrored into the archive.
41. **Header shows total build-system disk usage**, not a per-session delta. The old
    `disk +0B`-on-reopen was useless; the header now reports the whole build dir's on-disk size.
42. **Sections fill the available space + live resize.** Multi-section tabs no longer collapse
    items while the screen is half-empty: the visible height is shared fairly across sections
    (water-fill), and the layout re-flows the instant the terminal is resized. The focused
    section's selected item is *always* kept on-screen (a `_layout_sections` planner that provably
    fits the body — verified by a 330k-case sweep).
43. **Never resurrect a broken dependency venv.** A venv whose `pip install` failed (e.g. an
    unpublished pin) leaves `bin/python` with no packages; it must be rebuilt, not reused. Readiness
    is gated on a `.gflib-installed` success marker, so a half-installed cohort venv is rebuilt on
    the next run instead of failing every family with “No module named gftools”.
44. **Auto-retry transient clone failures.** `fetch-pack: invalid index-pack output` and similar
    network hiccups are retried a few times (abortable backoff); permanent errors (repo
    missing/private) are not.
45. **Failure-cause summary.** Failures are grouped by cause with an actionable hint (broken venv →
    rebuilt next run; transient fetch → retried; stale mirror → `git remote update`; …) — shown in
    the *failures* tab, the status panel, and the completion banner.
46. **Completion / stopped banner.** When the build finishes or the daemon dies, a prominent banner
    states the outcome (built/failed/skipped of N), the top failure cause, and next steps — so the
    dashboard never just looks frozen mid-flight.
47. **Coherent counts.** Families left `queued`/`building` by a prior run that aren't in the current
    worklist (e.g. queued at a higher `--percent`, now outside the sample) are reconciled to
    `skipped (not selected this run)`, so the counters reflect real pending work.
48. **Proactive self-healing — re-attempt fixable failures.** Starting a build automatically
    *retries* families that failed with a cause a fresh try can clear (broken venv, dependency
    install, transient fetch, stale mirror, …), so pressing `[C]` → Start
    actually moves things forward instead of instantly declaring the build complete. Genuine build
    errors / unreachable repos are kept (they'd just re-fail) unless you tick **retry ALL failed**
    in the config tab. A retried family rebuilds its broken venv from scratch. (A cause that needs
    a human — a missing system `-dev` library — is *not* auto-retried; fix it then use retry-all.)
49. **`[R]` — retry the selected family now.** Select a family (e.g. in the *failures* tab) and
    press `R` to re-attempt just that one — a single, non-disruptive in-process action. The
    detached daemon **lingers after the build completes** (status writer + control watcher stay
    alive), so `[R]` re-queues the family live with the lists and the whole UI unchanged — no
    program reload. The daemon idle-exits after 30 min of no new work.
50. **Hermetic, self-correcting venvs.** A venv's readiness is keyed to a hash of its requirements,
    so an empty/stale/wrong install is rebuilt (never reused). `base_requirements` is re-derived
    from the bundled file each run (never persisted as an absolute path that breaks across
    machines). setuptools+wheel are seeded (Py3.12+ omit them). Dependency *conflicts* (a cohort
    needing a different version of a base pin) are auto-relaxed cohort-locally.
51. **Meaningful failure causes.** No more bare `pip install rc=1`: failures are classified as
    dependency conflict, pip-resolution-too-deep, build-needs-setuptools, missing-system-library
    (with the `apt install` hint), misconfigured-requirements, transient fetch, stale mirror, etc.
52. **Progress = processed / selected.** The bar counts built + failed + skipped (everything that
    won't be retried this pass), so it reaches 100% when nothing is queued/building — labelled
    `N/M processed (P%)`.
53. **Cohort rows list family names** separated by ` | ` in a distinct colour from the count/key.
54. **Stable selection.** As a list grows / shrinks / reorders (families moving failed → building
    → built live), the cursor stays on the *same item*, not the same row index.
55. **Per-family pre-build commands** (`build_rules.json`, version-controlled next to the script).
    Map a family slug to ordered shell commands that run — with `cwd` = the extracted upstream
    source and the build venv's bin first on `PATH` — *before* gftools-builder / fontc / fontmake,
    for families whose sources must be generated/pre-compiled first. Auto-detected, or
    `--build-rules PATH`. A non-zero exit fails the family with a clear `pre-build` error.
56. **Pinned "Now building".** While families are compiling, the live list of what's building is
    shown on *every* tab (not just overview), so you never lose sight of it.
57. **Defaults & readability.** Lands on the **overview** tab; the **Failures** list shows *all*
    currently-failed families (matching the count); config paths render relative to where you
    launched; the progress bar is **colour-coded by outcome** (built/failed/skipped); tables use
    distinct, subtle per-column colours; transient (network/IO) failures **auto-retry in-build**;
    and each failure-cause bucket names *which* families it affected.
58. **Output collection in any `outputDir`.** Built fonts are found by a recursive, freshly-built-only
    scan of the work tree (not a fixed shallow list), so a build that writes to a custom `outputDir`
    (e.g. `fonts/<Family>/variable/`) is no longer a false `produced no expected font files`; a real
    name mismatch is reported as `output name mismatch` instead.
59. **Re-trigger a set / priority queue.** `--only` takes a comma list **or `@file`** (one slug per
    line) and restricts the whole run to those families — they become the entire queue (highest
    priority). `--retry-category "output name mismatch"` re-attempts only failures of that cause
    (like `--retry-failed`, but targeted — e.g. after fixing `collect_outputs`).
62. **Persistent across restarts.** The cohort→venv cache is shown (● = venv on disk, reused next
    run); the failure history is durable (`failure-history.jsonl`, append-only) + the failing log is
    archived to `logs/failed/`, surfaced in a *Failure history* section that survives restarts and
    re-attempts.
63. **Archive tab.** Shows the **total repos in the whole archive on disk** (not a session count) and a
    queue-oriented, colour-coded **multi-column** grid: *cloning now* (yellow), *recently archived,
    last 30 min* (green), *queued next* (cyan), plus an *unreachable* list with the git reason.
64. **Responsive monitor.** The dashboard re-parses `status.json` only when it changes (mtime-gated),
    keeping the UI snappy even on networked filesystems.
60. **Queue tab.** A `queue` tab lists the waiting families in priority order, each tagged **new**
    (never built), **retry** (after a failure), or **rebuild** (of a prior success) — the only three
    kinds a family can be queued under.
61. **Web dashboard (`--ui web`).** A browser UI that mirrors the TUI: it serves the same `snapshot()`
    at `/api/status` and routes live controls (jobs / percent / retry / pause) to `control.json` via
    `/api/control` — the same channel the curses monitor uses. All tabs (overview, queue, cohorts,
    built, failures, stats, config), the segmented progress bar, pinned "Now building", per-row detail
    and the control log are rendered from the live snapshot, polled every 1.5 s. Pure stdlib
    (`http.server`); `--web-port` sets the port (default 8765).

---
