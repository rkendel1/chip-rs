#!/usr/bin/env python3
"""Summarise `bench-frontier.py` results: paired control-versus-frontier tables, in Markdown.

    python3 -I scripts/bench-frontier-report.py RESULTS.json [RESULTS.json ...]

A pair is (model, task, repeat) with both arms. Runs that ended on the strict-JSON boundary (`format`) or
on the provider (`provider`) say nothing about decision quality and are counted separately; the decision
comparison uses only pairs in which both arms reached a real decision loop (`ok`).
"""
import json, statistics, sys
from collections import defaultdict

ARMS = ("control", "frontier")


def load(paths):
    rows = []
    for p in paths:
        rows += json.load(open(p))
    return rows


def pairs(rows):
    by = defaultdict(dict)
    for r in rows:
        by[(r["model"], r["task"], r.get("rep", 0))][r["evaluation_arm"]] = r
    return by


def pct(a, b):
    return "n/a" if not b else f"{100.0 * (a - b) / b:+.1f}%"


def main(paths):
    rows = load(paths)
    by = pairs(rows)
    models = sorted({k[0] for k in by})
    out = []
    for model in models:
        mp = {k: v for k, v in by.items() if k[0] == model and set(v) == set(ARMS)}
        out.append(f"### {model}\n")
        # failure classes
        cls = {arm: defaultdict(int) for arm in ARMS}
        for v in mp.values():
            for arm in ARMS:
                cls[arm][v[arm]["failure_class"]] += 1
        classes = sorted({c for arm in ARMS for c in cls[arm]})
        out.append("| run class | control | frontier |\n| --- | --- | --- |")
        for c in classes:
            out.append(f"| {c} | {cls['control'][c]} | {cls['frontier'][c]} |")
        out.append("")
        equiv = [v["control"].get("start_equivalent") for v in mp.values() if v["control"].get("model_calls") is not None or v["control"]["failure_class"] == "format"]
        out.append(f"Both arms began from the same request but for the frontier section in {sum(1 for e in equiv if e)} of {len(equiv)} pairs that reached the model.\n")
        decide = {k: v for k, v in mp.items() if all(v[a]["failure_class"] == "ok" for a in ARMS)}
        out.append(f"**Decision pairs** (both arms reached a real decision loop): {len(decide)} of {len(mp)}.\n")
        if decide:
            def tot(arm, key):
                return sum((v[arm].get(key) or 0) for v in decide.values())
            out.append("| metric (sum over decision pairs) | control | frontier | change |\n| --- | --- | --- | --- |")
            for label, key in [("model calls", "model_calls"), ("executions", "executions"), ("wrong valid decisions", "wrong_valid"),
                               ("supporting decisions", "supporting"), ("recovery executions", "recovery_executions"),
                               ("failed executions", "failed_observations"), ("tokens (provider-reported)", "tokens"),
                               ("request bytes", "total_request_bytes"), ("model latency ms", "model_latency_ms")]:
                a, b = tot("control", key), tot("frontier", key)
                out.append(f"| {label} | {a:g} | {b:g} | {pct(b, a)} |")
            vc = sum(1 for v in decide.values() if v["control"]["verified"])
            vf = sum(1 for v in decide.values() if v["frontier"]["verified"])
            out.append(f"| verified runs | {vc} | {vf} | |")
            uc = statistics.mean([(v["control"]["useful_per_call"] or 0) for v in decide.values()])
            uf = statistics.mean([(v["frontier"]["useful_per_call"] or 0) for v in decide.values()])
            out.append(f"| mean verified useful work per model call | {uc:.3f} | {uf:.3f} | |")
            out.append("")
            for label, key, lower_better in [("wrong valid decisions", "wrong_valid", True), ("recovery executions", "recovery_executions", True),
                                             ("model calls", "model_calls", True), ("executions", "executions", True)]:
                better = same = worse = 0
                for v in decide.values():
                    d = v["frontier"][key] - v["control"][key]
                    if d == 0:
                        same += 1
                    elif (d < 0) == lower_better:
                        better += 1
                    else:
                        worse += 1
                out.append(f"- {label}: frontier better in {better}, same in {same}, worse in {worse} of {len(decide)} pairs")
            vb = sum(1 for v in decide.values() if v["frontier"]["verified"] and not v["control"]["verified"])
            vw = sum(1 for v in decide.values() if v["control"]["verified"] and not v["frontier"]["verified"])
            out.append(f"- verified: frontier verified where control did not in {vb}; control verified where frontier did not in {vw}")
            out.append("")
        # context cost on every pair that reached the model
        cost = [v["frontier"]["first_call_input_tokens"] - v["control"]["first_call_input_tokens"] for v in mp.values()
                if v["control"].get("first_call_input_tokens") and v["frontier"].get("first_call_input_tokens")]
        if cost:
            out.append(f"Added prompt tokens on the first call (identical request but for the section): median {statistics.median(cost):g}, range {min(cost):g} to {max(cost):g}, over {len(cost)} pairs.\n")
        # safety
        ran = [v[a] for v in mp.values() for a in ARMS if v[a].get("audit_clean") is not None]
        out.append(
            f"Safety over {len(ran)} runs that executed: audit clean in {sum(1 for r in ran if r['audit_clean'])}; "
            f"false completions {sum(r['false_completions'] for r in ran)}; unauthorized {sum(r['unauthorized'] for r in ran)}; "
            f"capability violations {sum(r['capability_violations'] for r in ran)}; evidence violations {sum(r['evidence_violations'] for r in ran)}.\n"
        )
        # repeat consistency
        groups = defaultdict(list)
        for r in rows:
            if r["model"] == model and r.get("model_calls") is not None:
                groups[(r["task"], r["evaluation_arm"])].append((r["model_calls"], r["executions"], r["verified"]))
        multi = [g for g in groups.values() if len(g) > 1]
        same = sum(1 for g in multi if len(set(g)) == 1)
        out.append(f"Repeat consistency: {same} of {len(multi)} (task, arm) cells gave an identical result on both repeats.\n")
        # per task
        out.append("| task | control (calls/execs/wrong/recovery/verified) | frontier (calls/execs/wrong/recovery/verified) |\n| --- | --- | --- | ")
        for (m, t, rep), v in sorted(mp.items()):
            if rep != 0:
                continue
            f = lambda r: ("-" if r.get("model_calls") is None else f"{r['model_calls']}/{r['executions']}/{r['wrong_valid']}/{r['recovery_executions']}/{'Y' if r['verified'] else 'n'}") + f" [{r['failure_class']}]"
            out.append(f"| {t} | {f(v['control'])} | {f(v['frontier'])} |")
        out.append("")
    print("\n".join(out))


if __name__ == "__main__":
    main(sys.argv[1:])
