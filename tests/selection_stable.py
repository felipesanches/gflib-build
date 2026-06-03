"""Selection stability: the dashboard keeps the SAME item selected (not the same index) as a list
grows / shrinks / reorders live. Unit-tests CursesFrontend._item_key + _resolve_selection, which
the _draw clamp uses every frame."""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

K = g.CursesFrontend._item_key
R = g.CursesFrontend._resolve_selection

# ---- _item_key: identity by slug / key / cat / name / tuple head ----
assert K({"slug": "ofl/a", "error": "x"}) == "ofl/a"
assert K({"key": "base", "count": 2}) == "base"
assert K({"cat": "broken venv", "count": 5}) == "broken venv"
assert K({"name": "build", "status": "running"}) == "build"
assert K(("clone_gf", 12.3)) == "clone_gf"
assert K("plain") is None
print("_item_key: stable identity per item kind OK")


def F(s):
    return {"slug": s, "error": "e"}


# ---- a new item shifts the list: selection FOLLOWS the same item ----
items = [F("a"), F("KEEP"), F("c")]
sel, key = R(items, 1, None)                 # first land on index 1 (no prior key)
assert sel == 1 and key == "KEEP", (sel, key)
items2 = [F("new"), F("a"), F("KEEP"), F("c")]   # KEEP shifted 1 -> 2
sel, key = R(items2, sel, key)
assert sel == 2 and key == "KEEP", (sel, key)
print("insert ahead: selection followed KEEP from index 1 to 2")

# ---- the selected item is removed: selection stays at the (clamped) index, new identity adopted ----
items3 = [F("a"), F("c")]                     # KEEP gone
sel, key = R(items3, sel, key)
assert sel == 1 and key == "c", (sel, key)   # clamped to last; now tracking 'c'
print("removed selected item: clamped + re-bound to the item now under the cursor")

# ---- a fresh move (sel_key=None) is NOT undone by re-resolve ----
items4 = [F("a"), F("b"), F("c")]
sel, key = R(items4, 2, None)                # user just pressed down to index 2
assert sel == 2 and key == "c", (sel, key)
# next frame, list unchanged, key set -> stays on c
sel, key = R(items4, sel, key)
assert sel == 2 and key == "c"
print("a just-made move is preserved (not snapped back)")

# ---- empty list ----
sel, key = R([], 3, "whatever")
assert sel == 0 and key is None
print("empty list: sel 0, no key")

print("\nSELECTION-STABLE OK")
