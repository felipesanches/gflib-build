#!/usr/bin/env bash
# Run gflib-build (the official Rust implementation) on the laptop, NATIVELY (outside the dev VM),
# against the canonical build dir. It:
#   1. stops any running build daemon (only ONE builder may own the build dir at a time),
#   2. backs up the resumable state as a safety net (so a bad run can be rewound),
#   3. runs the build with a live TUI, reusing the existing cohort venvs + already-built fonts.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"   # repo root (works regardless of the host mount path)
cd "$REPO"
BUILD="gflib-data/build"
RUST="./rust/target/release/gflib-build"
BK="$HOME/gflib-statebackup-$(date +%Y%m%d-%H%M%S)"

# 0. sanity: the binary must run on THIS machine. If it was built in the VM and the host glibc
#    differs, rebuild it natively first.
if ! "$RUST" --help >/dev/null 2>&1; then
  echo "!! $RUST does not run here — build it natively on the host first:"
  echo "     (cd $REPO/rust && cargo build --release)"
  exit 1
fi

echo "== 1. stopping any running build daemon (if any) =="
"$RUST" --stop --build-dir "$BUILD" 2>/dev/null || true
sleep 3
if [ -f "$BUILD/daemon.pid" ] && kill -0 "$(cat "$BUILD/daemon.pid")" 2>/dev/null; then
  echo "!! a daemon still holds $BUILD (pid $(cat "$BUILD/daemon.pid")). Stop it before continuing."; exit 1
fi

echo "== 2. backing up resumable state -> $BK =="
mkdir -p "$BK"
for f in state.json status.json failure-history.jsonl events.jsonl timings.json migration.json; do
  cp -a "$BUILD/$f" "$BK/" 2>/dev/null || true
done
cp -a gflib-data/gflib-build.config "$BK/gflib-build.config" 2>/dev/null || true
echo "$BUILD" > "$BK/_build_dir.txt"
echo "   state snapshot saved (delete it once you're happy: rm -rf $BK)"

echo "== 3. running the build on $BUILD (live TUI; reuses the cohort venvs) =="
echo "   (press q to quit the dashboard — the build keeps running detached; --stop to cancel)"
echo
# --jobs: native host has no virtiofs penalty; tune to taste. Start modest.
# --mirror-missing: the archive pre-warmer clones any not-yet-mirrored upstream repos (append-only)
#   so the whole library archive is filled concurrently with the build. For a quick first smoke test
#   that clones nothing, append e.g.  --only ofl/brawler  or  --percent 2  and drop --mirror-missing.
exec "$RUST" \
  --data-dir gflib-data --build-dir "$BUILD" \
  --source metadata --google-fonts gflib-data/google-fonts --archive gflib-data/archive \
  --manage-venvs --base-requirements requirements-build.txt --base-python python3 \
  --mirror-missing --jobs 6 "$@"
