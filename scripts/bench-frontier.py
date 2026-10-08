#!/usr/bin/env python3
"""Does showing the model the Decision Frontier improve its decisions?

    python3 -I scripts/bench-frontier.py --chip BIN --out DIR [--repo PATH]
        [--models haiku,qwen-vllm,qwen-ollama] [--tasks all|F1,F4,I3,...] [--repeat N]

One binary, two arms. The control is Chip's existing model context. The treatment is the same context
plus the Decision Frontier as read-only text (`CHIP_FRONTIER_CONTEXT=true`). The model, provider, prompt,
goal, capabilities, project, limits and PAX are identical; the harness proves that for every pair by
comparing the first request each arm sent after removing the frontier section. The arm name is benchmark
metadata kept here; it is never part of Chip's work result.

A reply that is not a valid Chip decision (the strict JSON boundary) ends a run on its first turn in either
arm. Those runs are classified as `format` failures and reported separately: they say nothing about
decision quality. Provider failures are `provider`. Only `ok`/`limit`/`blocked` runs measure decisions.

Models need `CHIP_API_KEY` in the environment for Anthropic; nothing here prints or stores it.
"""
import argparse, importlib.util, json, os, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("bench_observe", os.path.join(HERE, "bench-observe.py"))
b = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b)  # the proxy, models and the real-workspace fixture

SECTION_HEAD = "Decision frontier:\n"


# ---- fixtures: a multi-module crate with up to three independent bugs ------------------------------------
def shop(root, bugs):
    """Eight modules; each bug fails exactly one test, in a different module."""
    b.write(root, "Cargo.toml", '[package]\nname = "shop"\nversion = "0.1.0"\nedition = "2021"\n')
    b.write(root, "src/lib.rs", "pub mod audit;\npub mod catalog;\npub mod customer;\npub mod invoice;\npub mod pricing;\npub mod shipping;\npub mod tax;\npub mod util;\n")
    b.write(root, "src/util.rs", "pub fn percent_of(amount: u64, bps: u64) -> u64 {\n    amount * bps / 10_000\n}\n")
    b.write(root, "src/audit.rs", "pub struct AuditEntry {\n    pub action: String,\n}\n\npub fn record(action: &str) -> AuditEntry {\n    AuditEntry { action: action.to_string() }\n}\n")
    b.write(root, "src/catalog.rs", "pub struct Item {\n    pub sku: &'static str,\n    pub unit_cents: u64,\n}\n\npub fn lookup(sku: &str) -> Option<Item> {\n    match sku {\n        \"pen\" => Some(Item { sku: \"pen\", unit_cents: 200 }),\n        _ => None,\n    }\n}\n")
    b.write(root, "src/customer.rs", "pub fn is_business(name: &str) -> bool {\n    name.ends_with(\"Ltd\")\n}\n")
    b.write(root, "src/invoice.rs", "use crate::{pricing, shipping, tax};\n\npub fn total_cents(unit: u64, qty: u32, region: &str) -> u64 {\n    let before_tax = pricing::line_cents(unit, qty) + shipping::flat_cents(region);\n    before_tax + tax::tax_cents(before_tax, region)\n}\n")
    b.write(root, "src/pricing/mod.rs", "pub mod discount;\n\npub fn line_cents(unit: u64, qty: u32) -> u64 {\n    discount::bulk(qty, unit * qty as u64)\n}\n")
    cmp = ">" if "discount" in bugs else ">="
    b.write(root, "src/pricing/discount.rs", f"use crate::util::percent_of;\n\n/// Orders of ten or more units get ten percent off the line.\npub fn bulk(qty: u32, line: u64) -> u64 {{\n    if qty {cmp} 10 {{\n        line - percent_of(line, 1_000)\n    }} else {{\n        line\n    }}\n}}\n")
    eu = 1900 if "tax" in bugs else 2000
    b.write(root, "src/tax.rs", f"use crate::util::percent_of;\n\n/// Tax in basis points for a shipping region.\npub fn rate_bps(region: &str) -> u64 {{\n    match region {{\n        \"EU\" => {eu},\n        \"US\" => 700,\n        _ => 0,\n    }}\n}}\n\npub fn tax_cents(amount: u64, region: &str) -> u64 {{\n    percent_of(amount, rate_bps(region))\n}}\n")
    ship = 700 if "shipping" in bugs else 900
    b.write(root, "src/shipping.rs", f"/// Flat shipping in cents for a region.\npub fn flat_cents(region: &str) -> u64 {{\n    match region {{\n        \"EU\" => {ship},\n        _ => 500,\n    }}\n}}\n")
    b.write(root, "tests/modules.rs",
            "use shop::{pricing::discount, shipping, tax};\n\n"
            "#[test]\nfn eu_tax_is_twenty_percent() {\n    assert_eq!(tax::tax_cents(10_000, \"EU\"), 2_000);\n}\n\n"
            "#[test]\nfn ten_units_get_the_bulk_discount() {\n    assert_eq!(discount::bulk(10, 10_000), 9_000);\n}\n\n"
            "#[test]\nfn eu_shipping_is_nine_dollars() {\n    assert_eq!(shipping::flat_cents(\"EU\"), 900);\n}\n")
    b.git_init(root)


def change(*bugs):
    return dict(kind="change", fixture=lambda r, repo, bugs=set(bugs): shop(r, bugs),
                goal="The project's tests fail. Make them pass.", expect=[])


TASKS = {
    "F1": change("tax"), "F2": change("discount"), "F3": change("shipping"),
    "F4": change("tax", "discount"), "F5": change("tax", "shipping"),
    "F6": change("discount", "shipping"), "F7": change("tax", "discount", "shipping"),
    "I3": b.TASKS["I3"], "I4": b.TASKS["I4"],
}


# ---- the equivalence proof ---------------------------------------------------------------------------------
def strip_section(x):
    """The same value with the frontier section removed from every string in it."""
    if isinstance(x, str):
        if SECTION_HEAD not in x:
            return x
        head, rest = x.split(SECTION_HEAD, 1)
        return head + rest[rest.index("Question:"):]
    if isinstance(x, list):
        return [strip_section(i) for i in x]
    if isinstance(x, dict):
        return {k: strip_section(v) for k, v in x.items()}
    return x


def first_request(trace):
    try:
        return json.loads(open(trace).readline())["request"]
    except Exception:
        return None


def has_section(req):
    return req is not None and SECTION_HEAD in json.dumps(req).replace("\\n", "\n")


# ---- a run ---------------------------------------------------------------------------------------------------
def classify(j, code, stderr):
    if j is None:
        return "provider" if code == 3 else "error"
    reason = j.get("outcome_reason") or ""
    if "not a valid decision" in reason or "decision rejected" in reason:
        return "format"
    if "did not answer" in reason or "provider error" in reason:
        return "provider"
    return "ok"


def run_one(binary, task, model, arm, out, repo, tag):
    cfg, spec_ = b.MODELS[model], TASKS[task]
    root = os.path.join(out, tag, "project")
    shutil.rmtree(os.path.join(out, tag), ignore_errors=True)
    os.makedirs(root)
    spec_["fixture"](root, repo)
    trace = os.path.join(out, tag, "trace.jsonl")
    proxy = b.Proxy(cfg["upstream"], trace)
    env = dict(os.environ, **cfg.get("env", {}))
    env.pop("CHIP_ENABLE_PROJECT_OBSERVE", None)  # explicit and off, in both arms
    env.pop("CHIP_FRONTIER_CONTEXT", None)
    if arm == "frontier":
        env["CHIP_FRONTIER_CONTEXT"] = "true"
    cmd = [binary, "work", "--kind", spec_["kind"], "--provider", cfg["provider"], "--model", cfg["model"],
           "--endpoint", f"http://127.0.0.1:{proxy.port}{cfg['path']}", "--json", spec_["goal"]]
    start = time.time()
    p = subprocess.run(cmd, cwd=root, env=env, capture_output=True, text=True, timeout=900)
    wall = time.time() - start
    proxy.close()
    try:
        j = json.loads(p.stdout)
    except Exception:
        j = None
    row = dict(task=task, model=model, evaluation_arm=arm, tag=tag, exit=p.returncode, wall_s=round(wall, 1),
               failure_class=classify(j, p.returncode, p.stderr))
    row["first_request"] = first_request(trace)
    if j is None:
        row["stderr"] = p.stderr[-200:]
        return row
    c, m, u = j["context"], j["measurement"], j["decisions"]
    f = j["frontier"]
    calls = c["calls"]
    row.update(
        terminal=j["terminal_state"], reason=(j.get("outcome_reason") or "")[:100],
        goal_satisfied=j["goal_satisfied"], verified=j["verified"], grounded=j.get("grounded"), exit_status=j["exit_status"],
        model_calls=m["model_calls"], executions=m["executions"], by_capability=j["executions_by_capability"],
        wrong_valid=u["wrong_valid"], supporting=u["supporting"], recovery_executions=u["recovery_executions"],
        recovery_turns=u["recovery_turns"], recoveries=j["recoveries"], failed_observations=j["failed_observations"],
        frontier=f, tokens=c["total_reported_tokens"],
        first_call_input_tokens=calls[0]["reported_input_tokens"] if calls else None,
        total_request_bytes=c["total_request_bytes"], model_latency_ms=m["model_latency_ms"],
        useful_per_call=j["useful_work_per_model_call"], useful_per_exec=j["useful_work_per_execution"],
        audit_clean=j["audit"]["clean"], false_completions=j["audit"]["false_completions"],
        unauthorized=j["audit"]["unauthorized_executions"] + j["audit"]["unauthorized_completions"],
        capability_violations=j["audit"]["execution_without_valid_request"],
        evidence_violations=j["audit"]["evidence_without_observation"] + j["audit"]["observation_without_execution"]
        + j["audit"]["frontier_without_evidence"] + j["audit"]["stale_evidence_reuse"],
        invalid_decisions=j["invalid_decisions"],
    )
    return row


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--chip", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--repo", default=os.getcwd())
    ap.add_argument("--models", default="haiku,qwen-vllm,qwen-ollama")
    ap.add_argument("--tasks", default="all")
    ap.add_argument("--repeat", type=int, default=1)
    a = ap.parse_args()
    tasks = list(TASKS) if a.tasks == "all" else a.tasks.split(",")
    os.makedirs(a.out, exist_ok=True)
    rows = []
    for model in a.models.split(","):
        for task in tasks:
            for rep in range(a.repeat):
                pair = {}
                for arm in ("control", "frontier"):
                    row = run_one(a.chip, task, model, arm, a.out, a.repo, f"{model}-{task}-{arm}-{rep}")
                    row["rep"] = rep
                    pair[arm] = row
                    rows.append(row)
                # Both arms must have begun from the same request but for the section.
                c, f = pair["control"]["first_request"], pair["frontier"]["first_request"]
                same = c is not None and f is not None and json.dumps(strip_section(f), sort_keys=True) == json.dumps(c, sort_keys=True)
                for r in pair.values():
                    r["start_equivalent"] = bool(same)
                    r["control_has_section"] = has_section(c)
                    r["treatment_has_section"] = has_section(f)
                    r.pop("first_request", None)
                print(json.dumps({
                    "model": model, "task": task, "rep": rep, "start_equivalent": bool(same),
                    "control": {k: pair["control"].get(k) for k in ("failure_class", "terminal", "model_calls", "executions", "wrong_valid", "recovery_executions", "verified", "tokens")},
                    "frontier": {k: pair["frontier"].get(k) for k in ("failure_class", "terminal", "model_calls", "executions", "wrong_valid", "recovery_executions", "verified", "tokens")},
                }), flush=True)
                with open(os.path.join(a.out, "results.json"), "w") as fh:
                    json.dump(rows, fh, indent=1)


if __name__ == "__main__":
    sys.exit(main())
