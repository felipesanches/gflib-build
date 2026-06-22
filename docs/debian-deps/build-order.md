# Archive-pure dependency burn-down (30 packages to build from source)

- 13 specialist crates.io crates (debcargo straight from the registry)
- 17 git-pinned font-domain crates (debcargo against a vendored/git checkout; version-encode the commit)
- 2 tool binaries (dh-cargo): fontc, gftools-builder

Everything else (~540 ecosystem crates) is reused from Debian's librust-*-dev. Build bottom-up
in THIS order (each crate's deps-in-set must already be local librust-*-dev or in Debian):

  1. `ascii-dag 0.4.2`  ·  crate (crates.io)  ·  deps-in-set: —
  2. `ascii_plist_derive 0.2.0`  ·  crate (git)  ·  deps-in-set: —
  3. `font-types 0.12.0`  ·  crate (crates.io)  ·  deps-in-set: —
  4. `read-fonts 0.40.1`  ·  crate (crates.io)  ·  deps-in-set: font-types
  5. `write-fonts 0.49.1`  ·  crate (crates.io)  ·  deps-in-set: font-types, read-fonts
  6. `fontdrasil 0.4.0`  ·  crate (git)  ·  deps-in-set: write-fonts
  7. `fea-rs 0.22.0`  ·  crate (git)  ·  deps-in-set: fontdrasil, write-fonts
  8. `fontir 0.5.0`  ·  crate (git)  ·  deps-in-set: fontdrasil, write-fonts
  9. `fontbe 0.5.0`  ·  crate (git)  ·  deps-in-set: fea-rs, fontdrasil, fontir, write-fonts
 10. `fontra2fontir 0.4.0`  ·  crate (git)  ·  deps-in-set: fontdrasil, fontir, write-fonts
 11. `glyphs-reader 0.5.0`  ·  crate (git)  ·  deps-in-set: ascii_plist_derive, fontdrasil, write-fonts
 12. `glyphs2fontir 0.6.0`  ·  crate (git)  ·  deps-in-set: fontdrasil, fontir, glyphs-reader, write-fonts
 13. `norad 0.18.4`  ·  crate (crates.io)  ·  deps-in-set: —
 14. `ufo2fontir 0.4.0`  ·  crate (git)  ·  deps-in-set: fea-rs, fontdrasil, fontir, glyphs-reader, norad, write-fonts
 15. `fontc 0.6.0`  ·  tool (dh-cargo bin)  ·  deps-in-set: fontbe, fontdrasil, fontir, fontra2fontir, glyphs2fontir, ufo2fontir, write-fonts
 16. `openstep-plist 1.1.0`  ·  crate (crates.io)  ·  deps-in-set: —
 17. `glyphslib 0.2.7`  ·  crate (crates.io)  ·  deps-in-set: openstep-plist
 18. `skrifa 0.43.2`  ·  crate (crates.io)  ·  deps-in-set: read-fonts
 19. `babelfont 0.2.0-pre`  ·  crate (git)  ·  deps-in-set: fontbe, fontc, fontdrasil, fontir, glyphslib, norad, skrifa, write-fonts
 20. `fontmerge 0.1.0`  ·  crate (git)  ·  deps-in-set: babelfont, fontdrasil
 21. `google-fonts-subsets 0.202602.1`  ·  crate (crates.io)  ·  deps-in-set: —
 22. `gftools 0.1.0`  ·  crate (git)  ·  deps-in-set: google-fonts-subsets, norad
 23. `google-fonts-axisregistry 0.4.18`  ·  crate (git)  ·  deps-in-set: —
 24. `google-fonts-languages 0.7.7`  ·  crate (crates.io)  ·  deps-in-set: —
 25. `interpolatable 1.1.1`  ·  crate (git)  ·  deps-in-set: —
 26. `serde_yaml_ng 0.10.0`  ·  crate (crates.io)  ·  deps-in-set: —
 27. `skera 0.1.1`  ·  crate (git)  ·  deps-in-set: font-types, skrifa, write-fonts
 28. `tilvisan 0.1.0`  ·  crate (git)  ·  deps-in-set: skrifa, write-fonts
 29. `ttf2woff2 0.10.4`  ·  crate (crates.io)  ·  deps-in-set: —
 30. `yeslogic-unicode-blocks 0.2.0`  ·  crate (crates.io)  ·  deps-in-set: —
