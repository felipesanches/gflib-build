#!/usr/bin/env python3
"""Generate the archive-pure dependency-packaging burn-down for the Rust toolchain that
gflib-build's true build-from-source Debian font packages Build-Depend on (gftools-builder + fontc).

Reads gftools-builder3's Cargo.lock, isolates the crates NOT already in Debian (git-sourced font crates
+ a curated set of specialist crates.io crates), topologically orders them (leaves first), and emits:
  - manifest.json   : machine-readable burn-down (each crate: source, kind, deps-in-set)
  - build-order.md  : the human-readable debcargo/dh-cargo order
The ~540 common ecosystem crates already in Debian as librust-*-dev are reused, not listed."""
import json, tomllib, sys
from pathlib import Path

LOCK = Path("/home/fsanches/compartilhado/gftools-builder3/Cargo.lock")
OUT = Path(__file__).resolve().parent

# crates.io crates NOT in Debian (verify on host with verify-debian.sh before debcargo). A "registry"
# source does NOT imply Debian has it — the fontations family is crates.io-published yet absent.
SPECIALIST_MISSING = {
    "openstep-plist", "norad", "serde_yaml_ng", "ttf2woff2", "ascii-dag",
    "google-fonts-languages", "google-fonts-subsets", "glyphslib", "yeslogic-unicode-blocks",
    "font-types", "read-fonts", "write-fonts", "skrifa",  # fontations: crates.io-published, NOT in Debian
}
# the two binary tool packages (dh-cargo), built on top of the library crates
TOOLS = {"fontc", "gftools-builder"}
# fontspector/QA crates: feature-gate OFF to keep them out of the build-deps graph
FONTSPECTOR = {"fontspector-checkapi","fontspector-checkhelper","fontspector-hotfix",
    "fontspector-profile-fontwerk","fontspector-profile-googlefonts","fontspector-profile-iso15008",
    "fontspector-profile-opentype","fontspector-profile-universal","sr-aef","shaperglot"}

lock = tomllib.loads(LOCK.read_text())
pkgs = {p["name"]: p for p in lock["package"]}

def is_git(p): return str(p.get("source","")).startswith("git+")
def deps_names(p): return [d.split()[0] for d in p.get("dependencies", [])]

# the set that needs from-scratch packaging: git-sourced crates + specialist-missing crates.io crates
to_pkg = set()
for name, p in pkgs.items():
    if name in FONTSPECTOR: continue            # feature-gated off
    if is_git(p) or name in SPECIALIST_MISSING:
        to_pkg.add(name)

# topological order (leaves first) over the subgraph restricted to to_pkg
order, seen, stack = [], set(), set()
def visit(n):
    if n in seen: return
    if n in stack: return  # cycle guard (shouldn't happen in a lockfile)
    stack.add(n)
    for d in deps_names(pkgs.get(n, {})):
        if d in to_pkg: visit(d)
    stack.discard(n); seen.add(n); order.append(n)
for n in sorted(to_pkg): visit(n)

def classify(n):
    p = pkgs[n]
    kind = "tool (dh-cargo bin)" if n in TOOLS else ("crate (git)" if is_git(p) else "crate (crates.io)")
    return {"crate": n, "version": p.get("version",""), "source": "git" if is_git(p) else "crates.io",
            "kind": kind, "deps_in_set": sorted(d for d in deps_names(p) if d in to_pkg)}

manifest = [classify(n) for n in order]
(OUT/"manifest.json").write_text(json.dumps({"count": len(manifest), "packages": manifest}, indent=2)+"\n")

git_n = sum(1 for m in manifest if m["source"]=="git")
cio_n = sum(1 for m in manifest if m["source"]=="crates.io" and m["crate"] not in TOOLS)
md = [f"# Archive-pure dependency burn-down ({len(manifest)} packages to build from source)",
      "",
      f"- {cio_n} specialist crates.io crates (debcargo straight from the registry)",
      f"- {git_n} git-pinned font-domain crates (debcargo against a vendored/git checkout; version-encode the commit)",
      f"- {len(TOOLS)} tool binaries (dh-cargo): fontc, gftools-builder",
      "",
      "Everything else (~540 ecosystem crates) is reused from Debian's librust-*-dev. Build bottom-up",
      "in THIS order (each crate's deps-in-set must already be local librust-*-dev or in Debian):",
      ""]
for i, m in enumerate(manifest, 1):
    deps = ", ".join(m["deps_in_set"]) or "—"
    md.append(f"{i:>3}. `{m['crate']} {m['version']}`  ·  {m['kind']}  ·  deps-in-set: {deps}")
(OUT/"build-order.md").write_text("\n".join(md)+"\n")
print(f"manifest: {len(manifest)} packages ({cio_n} crates.io, {git_n} git, {len(TOOLS)} tools)")
print("git-pinned crates:", [m["crate"] for m in manifest if m["source"]=="git"][:40])
