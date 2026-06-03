"""git_clone_mirror auto-retries TRANSIENT clone failures (e.g. 'fetch-pack: invalid index-pack
output') a few times before giving up, but does NOT retry permanent errors (repo missing/private)
or aborts. Unit-tests the retry policy by stubbing the single-attempt clone."""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import gflib_build as g

g.time.sleep = lambda *_a, **_k: None                 # don't actually wait during backoff

# ---- classifier ----
assert g.is_transient_clone_error("fatal: fetch-pack: invalid index-pack output")
assert g.is_transient_clone_error("error: RPC failed; curl 92 ...")
assert not g.is_transient_clone_error("remote: Repository not found")
assert not g.is_transient_clone_error("fatal: Authentication failed")
print("classifier: transient vs permanent OK")

calls = {"n": 0}


def make_once(seq):
    """seq is a list of (rc, aborted, err) returned in order; extra calls repeat the last."""
    calls["n"] = 0

    def _once(url, dest, timeout, stop=None):
        i = min(calls["n"], len(seq) - 1)
        calls["n"] += 1
        return seq[i]
    return _once


# ---- transient then success: retried, ends OK ----
g._clone_mirror_once = make_once([(128, False, "fatal: fetch-pack: invalid index-pack output"),
                                  (128, False, "fatal: fetch-pack: invalid index-pack output"),
                                  (0, False, "")])
rc, _, err = g.git_clone_mirror("u", "/d", timeout=5, stop=None, attempts=3)
assert rc == 0, (rc, err)
assert calls["n"] == 3, calls["n"]
print(f"transient x2 then success: retried {calls['n']} attempts, rc=0")

# ---- permanent error: NOT retried (single attempt) ----
g._clone_mirror_once = make_once([(128, False, "remote: Repository not found")])
rc, _, err = g.git_clone_mirror("u", "/d", timeout=5, stop=None, attempts=3)
assert rc != 0 and calls["n"] == 1, (rc, calls["n"])
assert "after" not in err                              # no "(after N attempts)" for a single try
print("permanent error: not retried (1 attempt)")

# ---- transient but always fails: exhausts attempts, notes the count ----
g._clone_mirror_once = make_once([(128, False, "fatal: fetch-pack: invalid index-pack output")])
rc, _, err = g.git_clone_mirror("u", "/d", timeout=5, stop=None, attempts=3)
assert rc != 0 and calls["n"] == 3, (rc, calls["n"])
assert "after 3 attempts" in err, err
print("transient always-fail: tried 3 attempts, error notes the count")

# ---- abort: no retry ----
g._clone_mirror_once = make_once([(1, True, "aborted")])
rc, _, err = g.git_clone_mirror("u", "/d", timeout=5, stop=None, attempts=3)
assert calls["n"] == 1, calls["n"]
print("aborted: not retried")

print("\nCLONE-RETRY OK")
