#!/usr/bin/env bash
# Roll back from the Rust port to the legacy Python tool, restoring the exact pre-Rust state.
# Usage:  rust/rollback-to-python.sh <snapshot-dir printed by run-on-host.sh>
set -euo pipefail

BK="${1:?usage: rollback-to-python.sh <snapshot dir from run-on-host.sh>}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
RUST="./rust/target/release/gflib-build"
BUILD="$(cat "$BK/_build_dir.txt" 2>/dev/null || echo gflib-data/build)"

echo "== stopping the Rust daemon (if running) on $BUILD =="
"$RUST" --stop --build-dir "$BUILD" 2>/dev/null || true
sleep 3

echo "== restoring the Python state from $BK =="
for f in state.json status.json failure-history.jsonl events.jsonl timings.json migration.json; do
  cp -a "$BK/$f" "$BUILD/$f" 2>/dev/null || true
done
cp -a "$BK/gflib-build.config" gflib-data/gflib-build.config 2>/dev/null || true

echo "== restarting the Python daemon (resumes from the restored state) =="
echo "   the cohort venvs + built fonts are shared (additive), so nothing was lost."
exec python3 gflib_build.py --build-dir "$BUILD"
