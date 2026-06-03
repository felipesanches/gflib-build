"""Exhaustive sweep of CursesFrontend._layout_sections — the multi-section stacking planner.
Asserts the two invariants that the earlier water-fill version violated (focused section's selected
item could be pushed off-screen): (A) the plan never uses more rows than `avail` (so a focused,
non-first section can never be clipped by earlier sections overrunning), and (B) the focused
section is always shown with its selected item inside the rendered window."""
import os
import sys
import itertools

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

plan_fn = g.CursesFrontend._layout_sections

SIZE_ALPHABET = [0, 1, 2, 3, 7, 40]
checked = 0
viol_overflow = []
viol_focus = []

for n in range(1, 5):
    for sizes in itertools.product(SIZE_ALPHABET, repeat=n):
        for fi in range(n):
            sized = sizes[fi]
            # sel is clamped by the caller to the focused section's item range before drawing
            sels = sorted(set([0, max(0, sized // 2), max(0, sized - 1)])) if sized else [0]
            for sel in sels:
                for avail in range(0, 26):
                    plan = plan_fn(avail, list(sizes), fi, sel)
                    checked += 1
                    # (A) total rows used must never exceed avail
                    used = sum(1 + e["count"] + (1 if e["hint"] else 0) + (1 if e["none"] else 0)
                               for e in plan)
                    if used > avail:
                        viol_overflow.append((avail, sizes, fi, sel, used))
                    # (B) when there is room (avail>=2) and the focused section has items, its
                    #     selected item must be within some shown section's window
                    if avail >= 2 and sized >= 1:
                        foc = [e for e in plan if e["idx"] == fi]
                        ok = bool(foc) and foc[0]["count"] >= 1 and \
                            foc[0]["top"] <= sel < foc[0]["top"] + foc[0]["count"]
                        if not ok:
                            viol_focus.append((avail, sizes, fi, sel, foc))
                    # every "(+N more)" hint must report a positive, correct hidden count
                    for e in plan:
                        if e["hint"]:
                            hidden = sizes[e["idx"]] - e["count"]   # items not shown (+N more)
                            if hidden < 1:
                                viol_focus.append(("hint", avail, sizes, fi, e))

print(f"checked {checked} layout configurations")
print(f"overflow violations: {len(viol_overflow)}")
print(f"focus-visibility violations: {len(viol_focus)}")
assert not viol_overflow, f"OVERFLOW e.g. {viol_overflow[:5]}"
assert not viol_focus, f"FOCUS HIDDEN e.g. {viol_focus[:5]}"

# spot-check the screenshot scenario: a focused later section is NOT clipped on a small screen
plan = plan_fn(6, [1, 1, 1], 2, 0)             # the exact high-sev repro (avail=6, n=3, focus last)
foc = [e for e in plan if e["idx"] == 2]
assert foc and foc[0]["count"] >= 1, f"focused last section hidden: {plan}"
print("high-sev repro (avail=6, 3 sections, focus the last) now shows the focused item:", foc[0])

# a big section expands to fill while a small one shows in full
plan = plan_fn(40, [5, 7, 0, 42], 0, 0)
by = {e["idx"]: e for e in plan}
assert by[1]["count"] == 7 and not by[1]["hint"], "small section should show in full"
assert by[3]["count"] >= 15, "large section should expand to fill the space"
print(f"fill: small section shows all 7; large section shows {by[3]['count']} of 42")

print("\nSECTION-LAYOUT OK")
