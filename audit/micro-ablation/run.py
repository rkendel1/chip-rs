#!/usr/bin/env python3
"""Controlled ablation harness for the shadow-mode micro-model (RIC-08 follow-up).

Evaluation infrastructure only: standard-library Python, not a workspace member, no production dependency, no
production behavior changed. It runs equivalent tasks through `chip work` under named configurations and scores
every run with an INDEPENDENT verification (a fresh PAX run on the final tree plus regression checks), never with
Chip's own verdict alone.

  A  deterministic baseline            chip work                      (no micro-model)
  B  baseline + micro-model shadow     chip work --micro-shadow       (observation only; outcomes must not change)
  C  micro-model may influence repairs NOT ENABLED. No integration with authority exists, and enabling one is a
                                       separate reviewed change. `--config C` reports "not_enabled" and exits 3.

Tasks are the executed, reproducible repository states of tests/fixtures/micro/fixture.json: each is a project
that fails its tests, with a known verified correction. The goal text is the same for every task and configuration.

Modes
  real              a work model from CHIP_PROVIDER/CHIP_MODEL/CHIP_ENDPOINT[/CHIP_API_KEY] (and, for B, a shadow
                    model from CHIP_MICRO_*). Without them the run is BLOCKED (exit 3) and reports no metrics.
  --scripted-dry-run  a scripted work model and a scripted heuristic stand in for models, to test THIS HARNESS
                    (including the A-versus-B outcome equivalence). The record says `scripted_dry_run` and
                    `evidence_about_a_model: false`. It is never a result about any model.

Reported per configuration and per failure class, with denominators and 95 % Wilson intervals: verified completion
(Chip said verified AND independent PAX passes AND no regression), zero-model verified completion, local-only
verified completion, regression rate, model calls and tokens per success, total task latency, shadow latency and
tokens, escalation rate and reasons. Measurements that cannot be made are `null` with a reason.

    python3 run.py --config A --scripted-dry-run --out /tmp/abl
    python3 run.py --config B --candidate qwen2.5-coder-1.5b-instruct --repeat 3 --out /tmp/abl
"""
import argparse, hashlib, json, math, os, shutil, subprocess, sys, tempfile, time
from urllib.parse import urlparse

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
FIXTURE_DIR = os.path.join(ROOT, "crates", "chip-cli", "tests", "fixtures", "micro")
sys.path.insert(0, os.path.join(ROOT, "audit", "hostile"))
GOAL = "Fix the failing tests in this project."
NO_LARGER_TIER = "chip work has no larger-model tier today (RIC-07b), so larger-model calls are structurally zero and cannot be compared"
NO_PRICE = "no price was configured (CHIP_PRICE_IN_PER_MTOK / CHIP_PRICE_OUT_PER_MTOK); a local model's cost is not measured"


def wilson(k, n):
    if n == 0:
        return {"k": 0, "n": 0, "low": None, "high": None}
    z = 1.959963984540054
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return {"k": k, "n": n, "low": max(0.0, c - h), "high": min(1.0, c + h)}


def rate(k, n):
    return {"rate": (k / n if n else None), **wilson(k, n)}


def pct(values, q):
    v = sorted(x for x in values if x is not None)
    return v[round((len(v) - 1) * q)] if v else None


def sh(cmd, cwd=None, env=None, timeout=600):
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)


def git(*a):
    p = sh(["git", *a], cwd=ROOT)
    return p.stdout.strip() if p.returncode == 0 else None


def repo_state():
    c = git("rev-parse", "HEAD")
    return {"git_commit": c, "working_tree_dirty": (bool(git("status", "--porcelain")) if c else None)}


def write_tree(root, files):
    for path, content in files.items():
        full = os.path.join(root, path)
        os.makedirs(os.path.dirname(full) or root, exist_ok=True)
        open(full, "w", encoding="utf-8").write(content)


def read_tree(root):
    out = {}
    for d, dirs, names in os.walk(root):
        dirs[:] = [x for x in dirs if x not in ("target", ".git")]
        for n in names:
            p = os.path.join(d, n)
            try:
                out[os.path.relpath(p, root)] = open(p, encoding="utf-8").read()
            except (UnicodeDecodeError, OSError):
                out[os.path.relpath(p, root)] = "<binary>"
    return out


def pax_test(project, target):
    env = dict(os.environ, CARGO_TARGET_DIR=target, RUST_BACKTRACE="0", CARGO_NET_OFFLINE="true")
    p = sh(["pax", "--json", "--dir", project, "test"], cwd=project, env=env)
    try:
        return json.loads(p.stdout)
    except json.JSONDecodeError:
        return {"status": "error", "reason": "unparseable-pax-output", "tests": None}


def code_lines(text):
    """The text without blank lines and whole-line comments, so a comment added to a file is not a changed test."""
    return "\n".join(l.rstrip() for l in text.splitlines() if l.strip() and not l.strip().startswith("//"))


def surface(files):
    s = {p: code_lines(c) for p, c in files.items() if p.startswith("tests/")}
    for p, c in files.items():
        i = c.find("#[cfg(test)]")
        if p.startswith("src/") and i >= 0:
            s[p] = code_lines(c[i:])
    return s


def load_tasks(only):
    fx = json.load(open(os.path.join(FIXTURE_DIR, "fixture.json"), encoding="utf-8"))
    tasks = []
    for c in fx["cases"]:
        r = c.get("repository") or {}
        if c["source"] != "native_capture" or not r.get("fix_files") or r.get("environment_override"):
            continue
        if only and c["id"] not in only:
            continue
        tasks.append({"id": c["id"], "split": c["split"], "category": c["category"],
                      "failure_class": ("unknown" if c["label"]["must_abstain"] else c["label"]["acceptable_classes"][0]),
                      "files": r["files"], "fix": r["fix_files"]})
    return fx, tasks


def class_of_url(url):
    host = urlparse(url).hostname or ""
    return host in ("127.0.0.1", "localhost", "::1")


# ---- scripted stand-ins (dry run only) -------------------------------------------------------------------------

def decision(capability, **inputs):
    d = {"decision": "request_capability", "capability": capability}
    if inputs:
        d["inputs"] = inputs
    return json.dumps(d)


def scripted_work(task, sabotage):
    steps = []
    if sabotage:
        path = sorted(task["fix"])[0]
        steps.append(decision("project.write", path=path, content=task["files"].get(path, "") + "\n// touched, not repaired\n"))
    else:
        steps += [decision("project.write", path=p, content=c) for p, c in sorted(task["fix"].items())]
    steps.append(decision("pax.test"))

    def script(n, text, req):
        return steps[min(n, len(steps) - 1)]
    return script


def scripted_micro(n, text, req):
    import re
    body = text[text.find("BEGIN SNAPSHOT"):]
    sid = re.search(r'"snapshot_id":"(snap-[0-9a-f]+)"', body)
    ver = re.search(r'"contract_version":(\d+)', body)
    diag = body
    head = {"schema": "chip.micro.v1", "contract_version": int(ver.group(1)) if ver else 0, "snapshot_id": sid.group(1) if sid else "", "relevant_scope": []}
    if "error[E" in diag:
        return json.dumps({**head, "outcome": "classified", "classification": "compile_error", "strategy_id": "narrow_edit", "applicability": "applicable"})
    if "assertion" in diag:
        return json.dumps({**head, "outcome": "classified", "classification": "test_assertion_failure", "strategy_id": "read_more_context", "applicability": "unknown"})
    return json.dumps({**head, "outcome": "abstained", "reason": "insufficient_evidence"})


# ---- one run ----------------------------------------------------------------------------------------------------

def run_one(cfg, task, rep, args, work_env, shadow_env, workdir, target, chip):
    project = os.path.join(workdir, f"{task['id']}-{cfg}-{rep}")
    write_tree(project, task["files"])
    ref_dir = os.path.join(workdir, f"{task['id']}-ref")
    if not os.path.isdir(ref_dir):
        write_tree(ref_dir, {**task["files"], **task["fix"]})
        task["reference"] = pax_test(ref_dir, target)
    before = pax_test(project, target)
    cmd = [chip, "work", "--json", "--max-turns", str(args.max_turns), "--max-executions", str(args.max_executions)]
    env = dict(os.environ, CARGO_TARGET_DIR=target, **work_env)
    if cfg == "B":
        cmd.append("--micro-shadow")
        env.update(shadow_env)
    cmd.append(GOAL)
    t0 = time.monotonic()
    p = sh(cmd, cwd=project, env=env, timeout=args.timeout)
    wall = (time.monotonic() - t0) * 1000
    try:
        report = json.loads(p.stdout)
    except json.JSONDecodeError:
        report = None
    after_files = read_tree(project)
    after = pax_test(project, target)
    ref_passed = ((task["reference"] or {}).get("tests") or {}).get("passed")
    passed_now = (after.get("tests") or {}).get("passed")
    regression = []
    if any(p_ not in after_files for p_ in task["files"]):
        regression.append("original_file_removed")
    if surface(after_files) != surface({**task["files"], **{}}) and surface(after_files) != surface({**task["files"], **task["fix"]}):
        regression.append("test_surface_changed")
    if after["status"] == "passed" and ref_passed is not None and (passed_now is None or passed_now < ref_passed):
        regression.append("fewer_passing_tests_than_reference")
    chip_verified = bool(report and report.get("verified"))
    independent = after["status"] == "passed"
    m = (report or {}).get("measurement") or {}
    return {
        "task": task["id"], "failure_class": task["failure_class"], "split": task["split"], "config": cfg, "rep": rep,
        "exit_code": p.returncode, "wall_ms": wall,
        "failed_before": before["status"] != "passed",
        "chip_verified": chip_verified, "independent_pass": independent, "regression": regression,
        "verified_completion": chip_verified and independent and not regression,
        "verification_disagreement": chip_verified != independent,
        "terminal_state": (report or {}).get("terminal_state"), "outcome_reason": (report or {}).get("outcome_reason"),
        "model_calls": m.get("model_calls"), "model_tokens": m.get("model_tokens"), "model_escalations": m.get("model_escalations"),
        "endpoint": (report or {}).get("endpoint"), "work_model": (report or {}).get("model"),
        "tree_sha256": hashlib.sha256(json.dumps(after_files, sort_keys=True).encode()).hexdigest(),
        "micro_shadow": (report or {}).get("micro_shadow"),
        "stderr_tail": p.stderr[-300:] if report is None else None,
    }


def summarize(runs, price):
    n = len(runs)
    ok = [r for r in runs if r["verified_completion"]]
    local = [r for r in ok if r["endpoint"] and class_of_url(r["endpoint"]) and not r["model_escalations"]]
    zero = [r for r in ok if r["model_calls"] == 0]
    toks = [r["model_tokens"] for r in ok]
    calls = [r["model_calls"] for r in ok]
    shadows = [r["micro_shadow"] for r in runs if r["micro_shadow"]]
    asked = [s for s in shadows if s.get("status") not in ("skipped",)]
    esc = [r for r in runs if r["terminal_state"] == "escalated"]
    reasons = {}
    for r in esc:
        reasons[r["outcome_reason"] or "unreported"] = reasons.get(r["outcome_reason"] or "unreported", 0) + 1
    out = {
        "runs": n,
        "verified_completion": rate(len(ok), n),
        "zero_model_verified_completion": rate(len(zero), n),
        "local_only_verified_completion": rate(len(local), n),
        "regression": rate(sum(1 for r in runs if r["regression"]), n),
        "regression_among_chip_verified": rate(sum(1 for r in runs if r["chip_verified"] and r["regression"]), sum(1 for r in runs if r["chip_verified"])),
        "verification_disagreements": sum(1 for r in runs if r["verification_disagreement"]),
        "larger_model_calls_per_success": {"value": None, "reason": NO_LARGER_TIER},
        "model_calls_per_success": (sum(calls) / len(calls)) if calls and None not in calls else None,
        "model_tokens_per_success": (sum(toks) / len(toks)) if toks and None not in toks else None,
        "tokens_unavailable_reason": None if (toks and None not in toks) else "no successful run, or the provider reported no token usage",
        "total_task_latency_ms": {"p50": pct([r["wall_ms"] for r in runs], 0.5), "p95": pct([r["wall_ms"] for r in runs], 0.95)},
        "escalation": {"rate": rate(len(esc), n), "reasons": reasons},
        "cost": ({"value": None, "reason": NO_PRICE} if price is None else
                 {"per_success": None if not ok or None in toks else sum(toks) * price / 1e6 / len(ok), "note": "tokens x configured price"}),
    }
    if shadows:
        out["micro_shadow"] = {
            "records": len(shadows), "asked": len(asked), "skipped": len(shadows) - len(asked),
            "status_counts": {s: sum(1 for x in shadows if x.get("status") == s) for s in sorted({x.get("status") for x in shadows})},
            "latency_ms": {"p50": pct([s.get("latency_ms") for s in asked], 0.5), "p95": pct([s.get("latency_ms") for s in asked], 0.95)},
            "mean_prompt_tokens": (lambda v: sum(v) / len(v) if v else None)([s["prompt_tokens"] for s in asked if s.get("prompt_tokens") is not None]),
            "mean_completion_tokens": (lambda v: sum(v) / len(v) if v else None)([s["completion_tokens"] for s in asked if s.get("completion_tokens") is not None]),
            "classification_accuracy": {"value": None, "reason": "the final failing state of an ablation run is not labelled; classification accuracy is measured on the fixture (micro_eval), not here"},
            "outcome_changed_by_shadow": 0 if all(s.get("authority") == "none" for s in shadows) else None,
        }
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", required=True, choices=["A", "B", "C"])
    ap.add_argument("--scripted-dry-run", action="store_true")
    ap.add_argument("--candidate")
    ap.add_argument("--tasks", nargs="*")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--max-turns", type=int, default=12)
    ap.add_argument("--max-executions", type=int, default=8)
    ap.add_argument("--timeout", type=int, default=900)
    ap.add_argument("--out", required=True)
    ap.add_argument("--chip", default=os.environ.get("CHIP_BIN", os.path.join(ROOT, "target", "debug", "chip")))
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    fx, tasks = load_tasks(args.tasks)
    base = {
        "schema": "chip.micro-ablation.v1", "config": args.config, "repository": repo_state(),
        "fixture": {"version": fx["fixture_version"], "sha256": "sha256:" + hashlib.sha256(open(os.path.join(FIXTURE_DIR, "fixture.json"), "rb").read()).hexdigest()},
        "goal": GOAL, "tasks": len(tasks), "repeat": args.repeat, "authority_granted": "none",
    }

    def stop(status, why, code):
        rec = {**base, "status": status, "reason": why, "evidence_about_a_model": False, "metrics": None}
        open(os.path.join(args.out, f"record-{args.config}.json"), "w").write(json.dumps(rec, indent=2))
        print(json.dumps(rec, indent=2))
        print(f"{status.upper()}: {why}", file=sys.stderr)
        sys.exit(code)

    if args.config == "C":
        stop("not_enabled", "configuration C needs an explicitly reviewed integration through which a micro-model nomination can influence strategy selection, validated by a Typed Repair Engine. Neither exists, and granting that authority is a separate reviewed change; nothing here enables it", 3)
    if not os.path.exists(args.chip):
        stop("blocked", f"chip binary not found at {args.chip}; build it (cargo build -p chip-cli)", 3)
    if sh(["pax", "--version"]).returncode != 0:
        stop("blocked", "PAX is not on PATH; the independent verification needs it", 3)

    scripted = args.scripted_dry_run
    work_env, shadow_env, servers = {}, {}, []
    if scripted:
        from mockmodel import Mock
    else:
        for var in ("CHIP_PROVIDER", "CHIP_MODEL", "CHIP_ENDPOINT"):
            if not os.environ.get(var):
                stop("blocked", f"no work model is configured ({var} is not set); nothing was run", 3)
        work_env = {k: os.environ[k] for k in os.environ if k.startswith("CHIP_") and not k.startswith("CHIP_MICRO_")}
        if args.config == "B":
            if not os.environ.get("CHIP_MICRO_MODEL") or not os.environ.get("CHIP_MICRO_ENDPOINT") and not os.environ.get("CHIP_MICRO_PROVIDER"):
                stop("blocked", "no shadow model is configured (CHIP_MICRO_*); nothing was run", 3)
            cands = json.load(open(os.path.join(FIXTURE_DIR, "candidates.json")))
            if args.candidate:
                entry = next((c for c in cands["candidates"] if c["key"] == args.candidate), None)
                names = [n.lower() for n in (entry or {}).get("accepted_model_names", [])]
                if entry is None or os.environ["CHIP_MICRO_MODEL"].lower() not in names:
                    stop("blocked", f"candidate {args.candidate} is not available: the configured shadow model ({os.environ['CHIP_MICRO_MODEL']}) is not one of its accepted names; no substitution", 3)
            shadow_env = {k: os.environ[k] for k in os.environ if k.startswith("CHIP_MICRO_")}
    price_in = os.environ.get("CHIP_PRICE_IN_PER_MTOK")
    price = float(price_in) if price_in else None

    workdir = tempfile.mkdtemp(prefix="micro-ablation-")
    target = os.path.join(workdir, "target")
    runs = []
    try:
        if scripted:
            micro_mock = Mock(scripted_micro)
            servers.append(micro_mock)
            shadow_env = {"CHIP_MICRO_PROVIDER": "openai-compatible", "CHIP_MICRO_MODEL": "scripted-heuristic", "CHIP_MICRO_ENDPOINT": micro_mock.url + "/v1/chat/completions"}
        for idx, task in enumerate(tasks):
            for rep in range(args.repeat):
                if scripted:
                    sabotage = idx % 3 == 2  # the same tasks fail under A and B, so the shadow path is exercised
                    m = Mock(scripted_work(task, sabotage))
                    servers.append(m)
                    work_env = {"CHIP_PROVIDER": "openai-compatible", "CHIP_MODEL": "scripted-work", "CHIP_ENDPOINT": m.url + "/v1/chat/completions"}
                r = run_one(args.config, task, rep, args, work_env, shadow_env, workdir, target, args.chip)
                runs.append(r)
                print(f"{r['task']:36} {args.config} rep{rep} chip_verified={r['chip_verified']} independent={r['independent_pass']} regression={r['regression']} "
                      f"state={r['terminal_state']} shadow={(r['micro_shadow'] or {}).get('status')}", file=sys.stderr)
    finally:
        for s in servers:
            s.close()
        shutil.rmtree(workdir, ignore_errors=True)

    classes = sorted({r["failure_class"] for r in runs})
    rec = {
        **base, "status": "scripted_dry_run" if scripted else "completed",
        "evidence_about_a_model": False if scripted else True,
        "note": "scripted work model and scripted heuristic: this tests the harness and says nothing about any model" if scripted else None,
        "work_model": None if scripted else {k: os.environ.get(k) for k in ("CHIP_PROVIDER", "CHIP_MODEL")} | {"endpoint_identity": urlparse(os.environ["CHIP_ENDPOINT"]).scheme + "://" + (urlparse(os.environ["CHIP_ENDPOINT"]).netloc.split("@")[-1])},
        "shadow_model": None if (scripted or args.config != "B") else {"model": os.environ.get("CHIP_MICRO_MODEL"), "provider": os.environ.get("CHIP_MICRO_PROVIDER")},
        "candidate": args.candidate,
        "settings": {"max_turns": args.max_turns, "max_executions": args.max_executions, "timeout_s": args.timeout},
        "metrics": {"overall": summarize(runs, price), "by_failure_class": {c: summarize([r for r in runs if r["failure_class"] == c], price) for c in classes}},
        "runs": runs,
    }
    path = os.path.join(args.out, f"record-{args.config}{'-dry' if scripted else ''}.json")
    open(path, "w").write(json.dumps(rec, indent=2))
    print(f"wrote {path}", file=sys.stderr)
    print(json.dumps(rec["metrics"]["overall"], indent=2))


if __name__ == "__main__":
    main()
