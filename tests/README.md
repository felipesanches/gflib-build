# tests

Headless regression checks for `gflib_build.py`. They monkeypatch the heavy operations
(clone / fontc build / discover / mirror / cohorts / per-family build) so the orchestration,
the live task-list, the curses rendering, and the detach/auto-attach flow can be validated in
seconds without real clones or compiles. Each script is self-locating (adds the repo root to
`sys.path`), so run them from anywhere:

```bash
python3 tests/smoke_tasklist.py     # driver walks the task-list; archive_recent grows; builds complete
python3 tests/pty_curses.py         # CursesFrontend renders the task-list/emoji/arrow-tabs in a pty, q exits cleanly
# detach_harness.py is a CLI harness invoked by hand to exercise --detach + auto-attach-on-rerun:
python3 tests/detach_harness.py --ui none --detach --yes --percent 100 \
    --data-dir /tmp/gfb-test --google-fonts /tmp/gfb-test/gf --archive /tmp/gfb-test/archive \
    --no-manage-venvs --no-save-config
```

Note: auto-attach-on-rerun relies on `read_daemon_pid`'s cmdline guard requiring
`gflib_build` in the process name; to exercise it with the harness, copy it to a filename
containing `gflib_build` (e.g. `cp tests/detach_harness.py gflib_build_th.py`).
