#!/usr/bin/env python3
"""Compares configuration A and B records of the same tasks and states the one thing shadow mode must satisfy:
execution outcomes identical. For scripted dry runs (deterministic work model) the check is exact, per task. For real
runs the work model is not deterministic, so exactness is not claimed: the per-run consistency of the shadow's own
`deterministic` copy with the process outcome is checked instead, and the A-versus-B difference is reported with
intervals and no significance claim.

    python3 compare.py record-A-dry.json record-B-dry.json
"""
import json, sys

OUTCOME = ("exit_code", "terminal_state", "chip_verified", "independent_pass", "regression", "model_calls", "tree_sha256", "outcome_reason")


def main(a_path, b_path):
    a, b = json.load(open(a_path)), json.load(open(b_path))
    scripted = a["status"] == "scripted_dry_run" and b["status"] == "scripted_dry_run"
    result = {"a": a_path, "b": b_path, "scripted_dry_run": scripted, "evidence_about_a_model": False if scripted else None, "problems": []}
    ar = {(r["task"], r["rep"]): r for r in a["runs"]}
    br = {(r["task"], r["rep"]): r for r in b["runs"]}
    if set(ar) != set(br):
        result["problems"].append("A and B do not cover the same tasks and repetitions")
    # per-run consistency of the shadow's copy of the deterministic outcome
    for key, r in br.items():
        s = r["micro_shadow"]
        if s is None:
            result["problems"].append(f"{key}: B run has no micro_shadow record")
            continue
        d = s["deterministic"]
        if d["exit_status"] != r["exit_code"] or d["verified"] != r["chip_verified"] or d["terminal_state"] != r["terminal_state"]:
            result["problems"].append(f"{key}: the shadow's deterministic copy differs from the process outcome")
        if s.get("authority") != "none":
            result["problems"].append(f"{key}: shadow record claims authority {s.get('authority')}")
    if scripted:
        for key in sorted(set(ar) & set(br)):
            diff = [f for f in OUTCOME if ar[key][f] != br[key][f]]
            if diff:
                result["problems"].append(f"{key}: A and B differ in {diff}")
    result["pairs_compared"] = len(set(ar) & set(br))
    result["identical_outcomes"] = not result["problems"] if scripted else None
    print(json.dumps(result, indent=2))
    sys.exit(1 if result["problems"] else 0)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
