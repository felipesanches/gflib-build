#!/usr/bin/env python3
"""Headless smoke test for the Rust TUI: starts the release binary as a read-only monitor against a
static (Python-schema) status.json in a pty, drives a few tabs, quits, and asserts it rendered the
header / disk wording / tabs / M0 provenance without panicking. Build first: `cargo build --release`.

Run:  python3 tui_smoke.py
"""
import os, sys, pty, time, select, struct, fcntl, termios, json, tempfile, subprocess

REPO = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(REPO, "target", "release", "gflib-build")
if not os.path.isfile(BIN):
    sys.exit("build first: (cd rust && cargo build --release)")

bd = tempfile.mkdtemp(prefix="_tui_rs_")
json.dump({
    "jobs": 8, "phase": "build", "elapsed": 300.0,
    "counts": {"built": 5, "failed": 2, "building": 1, "queued": 3},
    "backends": {"fontc": 4, "fontmake": 1},
    "disk_build_total": 1 << 30, "disk_archive_total": 2 << 30, "disk_free": 5 << 30,
    "builders": {"builder2": "gftools-builder2 0.9.74"}, "tooling": {"fontc": "fontc 0.9 (git abc)"},
    "building": [{"slug": "ofl/now", "worker": 1, "dur": 12.0, "backend": "fontc", "note": "checkout"}],
    "failures_recent": [{"slug": "ofl/boom", "error": "KeyError instances", "backend": "fontc",
                         "compiler_version": "fontc 0.9 (git abc)", "builder_version": "gftools-builder2 0.9.74"}],
    "built_recent": [{"slug": "ofl/ok", "backend": "fontc", "bytes": 2048, "compare": "identical",
                      "compiler_version": "fontc 0.9 (git abc)", "builder_version": "gftools-builder2 0.9.74"}],
    "config": {"jobs": 8, "backend": "auto", "source": "archive"},
}, open(os.path.join(bd, "status.json"), "w"))

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execvp(BIN, [BIN, "--attach", "--ui", "curses", "--build-dir", bd])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
out = b""


def drain(sec):
    global out
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                ch = os.read(fd, 65536)
            except OSError:
                return
            if not ch:
                return
            out += ch


drain(1.0)
for k in (b"\t", b"\t", b"\t", b"\x1b[B", b"\r", b"\x1b", b"q"):   # cycle tabs, open+close detail, quit
    os.write(fd, k); drain(0.3)
try:
    os.kill(pid, 9)
except OSError:
    pass
os.waitpid(pid, 0)
subprocess.run(["rm", "-rf", bd])

txt = out.decode("utf-8", "replace")
checks = {
    "no panic": "panicked" not in txt and "RUST_BACKTRACE" not in txt,
    "title": "Google Fonts library build" in txt,
    "disk wording (build + archive)": "build 1.0GiB + archive 2.0GiB" in txt,
    "tabs": "failures" in txt and "stats" in txt,
    "M0 provenance": "gftools-builder2 0.9.74" in txt,
}
for k, v in checks.items():
    print(f"  {'OK' if v else 'FAIL'}  {k}")
assert all(checks.values()), "TUI smoke test failed"
print("\nTUI-SMOKE OK")
