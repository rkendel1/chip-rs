"""Section 4: which escalation behaviours exist in the production path. Scripted adversary model."""
import json, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from runner import *
OUT = os.environ.get("AUDIT_OUT", "/home/user/chip-rs/docs/product/audit-evidence/hostile-audit")
GOAL = "Fix the failing tests in this project"


def seq(items, tail=None):
    return lambda n, t, r: items[n] if n < len(items) else (tail if tail is not None else complete("done"))


def brief(rec):
    c = rec.get("chip", {})
    m = c.get("measurement", {})
    return {"exit": rec["exit"], "terminal_state": c.get("terminal_state"), "verified": c.get("verified"), "outcome_reason": (c.get("outcome_reason") or "")[:160],
            "model_requests": rec["model_requests"], "pax_executions": c.get("pax_executions"), "executions": c.get("executions_by_capability"),
            "turns": m.get("turns"), "recoveries": c.get("recoveries"), "wall_s": rec["wall_s"]}


def main():
    out = {}
    wrong = call("project.write", path="src/money.rs", content=fixture.FILES["src/money.rs"])
    cases = {
        "terminal-completed": (seq([call("project.write", path="src/money.rs", content=fixed_text("src/money.rs")), call("project.write", path="src/account.rs", content=fixed_text("src/account.rs")), call("pax.test")]), {}),
        "terminal-blocked-by-model": (seq([block("the failing tests contradict the specification")]), {}),
        "terminal-escalated-by-model": (seq([escalate("two conventions disagree; a person must decide which")]), {}),
        "limit-turns": (lambda n, t, r: call("project.list", path="."), {}),
        "limit-executions-repeat-failing-test": (lambda n, t, r: call("pax.test"), {}),
        "repeat-identical-wrong-write-then-test": (seq([wrong, call("pax.test")] * 8), {}),
        "limits-lowered-to-3-turns": (lambda n, t, r: call("project.list", path="."), {"chip_args": ["--max-turns", "3"]}),
        "limit-ceiling-51-turns": (lambda n, t, r: call("project.list", path="."), {"chip_args": ["--max-turns", "51"]}),
    }
    for name, (script, kw) in cases.items():
        rec = run_work("e-" + name, GOAL, script, **kw)
        save(rec, os.path.join(OUT, "e-" + name + ".json"))
        out[name] = brief(rec)
        print(name, out[name])
    # goal size: the human's answer to an escalation can only travel inside a new goal
    long_goal = GOAL + " " + "x" * 2100
    d = os.path.join(ROOT, "runs", "e-goal-size", "proj"); fixture.make(d)
    p = subprocess.run([CHIP, "work", long_goal], cwd=d, env=base_env("http://127.0.0.1:9"), capture_output=True, text=True)
    out["goal-over-2000-bytes"] = {"exit": p.returncode, "stderr": p.stderr[-160:]}
    # instrumentation inventory: what a run reports
    rec = run_work("e-instrumentation", GOAL, seq([call("project.list", path=".")], tail=block("x")))
    c = rec["chip"]
    out["instrumentation"] = {"measurement_keys": sorted(c["measurement"].keys()), "context_keys": sorted(c["context"].keys()), "top_level_keys": sorted(c.keys()), "provider": c.get("provider"), "model": c.get("model")}
    json.dump(out, open(os.path.join(OUT, "summary-escalation.json"), "w"), indent=1)

main()
