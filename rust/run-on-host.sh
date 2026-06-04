#!/usr/bin/env bash
# Run the RUST gflib-build on the SAME build dir as the Python tool — safely and REVERSIBLY.
#
# Intended to run NATIVELY on the laptop (outside the VM). It:
#   1. stops the Python daemon (frees the build dir — only ONE builder may own it at a time),
#   2. backs up the resumable state so you can roll back to Python at any time,
#   3. runs the Rust port on the same build dir, REUSING the existing cohort venvs + built fonts.
#
# Roll back later with:  rust/rollback-to-python.sh <the snapshot dir this prints>
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"   # repo root (works regardless of the host mount path)
cd "$REPO"
BUILD="gflib-data/build"
RUST="./rust/target/release/gflib-build"
BK="$HOME/gflib-rollback-$(date +%Y%m%d-%H%M%S)"

# 0. sanity: the Rust binary must run on THIS machine. If it was built in the VM and the host glibc
#    differs, rebuild it natively first.
if ! "$RUST" --help >/dev/null 2>&1; then
  echo "!! $RUST does not run here — build it natively on the host first:"
  echo "     (cd $REPO/rust && cargo build --release)"
  exit 1
fi

echo "== 1. stopping the Python daemon (if running) =="
python3 gflib_build.py --stop --build-dir "$BUILD" 2>/dev/null || true
sleep 3
if [ -f "$BUILD/daemon.pid" ] && kill -0 "$(cat "$BUILD/daemon.pid")" 2>/dev/null; then
  echo "!! a daemon still holds $BUILD (pid $(cat "$BUILD/daemon.pid")). Stop it before continuing."; exit 1
fi

echo "== 2. backing up resumable state for rollback -> $BK =="
mkdir -p "$BK"
for f in state.json status.json failure-history.jsonl events.jsonl timings.json migration.json; do
  cp -a "$BUILD/$f" "$BK/" 2>/dev/null || true
done
cp -a gflib-data/gflib-build.config "$BK/gflib-build.config" 2>/dev/null || true
echo "$BUILD" > "$BK/_build_dir.txt"
echo "   snapshot saved. ROLL BACK with:  rust/rollback-to-python.sh $BK"

echo "== 3. running the Rust port on $BUILD (live TUI; reuses the cohort venvs) =="
echo "   (press q to quit the dashboard — the build keeps running detached; --stop to cancel)"
echo
# --jobs: native host has no virtiofs penalty; tune to taste. Start modest.
# --mirror-missing: clone any not-yet-mirrored upstream repos (append-only) so it can continue the
#   whole library, matching the Python run. For a quick first smoke test that clones nothing, append
#   e.g.  --only ofl/brawler  or  --percent 2  (extra args are forwarded) and drop --mirror-missing.
exec "$RUST" \
  --data-dir gflib-data --build-dir "$BUILD" \
  --source metadata --google-fonts gflib-data/google-fonts --archive gflib-data/archive \
  --manage-venvs --base-requirements requirements-build.txt --base-python python3 \
  --mirror-missing --jobs 6 "$@"
