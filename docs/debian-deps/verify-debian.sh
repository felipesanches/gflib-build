#!/bin/sh
# HOST helper: for every crates.io crate in gftools-builder3's Cargo.lock, report whether Debian ships
# librust-<crate>-dev. The MISSING ones must be added to SPECIALIST_MISSING in gen_manifest.py (the
# archive-pure burn-down). Run on the host (needs apt). Git-sourced crates are always from-scratch.
LOCK="${1:-/home/fsanches/compartilhado/gftools-builder3/Cargo.lock}"
echo "# crates.io crates MISSING from Debian (add these to the burn-down):"
python3 - "$LOCK" <<'PY' | while read c; do
import tomllib,sys
L=tomllib.loads(open(sys.argv[1],'rb').read().decode())
for p in L["package"]:
    if str(p.get("source","")).startswith("registry"): print(p["name"])
PY
  deb="librust-$(echo "$c" | tr '_' '-')-dev"
  apt-cache show "$deb" >/dev/null 2>&1 || echo "  $c  ->  $deb (MISSING)"
done
