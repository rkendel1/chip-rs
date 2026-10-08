#!/usr/bin/env python3
"""Matched baseline-versus-treatment benchmark for `project.observe`.

    python3 -I scripts/bench-observe.py --baseline BIN --treatment BIN --out DIR \
        [--repo PATH] [--models haiku,qwen-vllm,qwen-ollama] [--tasks all|I1,B1,...] [--repeat N]

The two binaries are the same code except that the baseline does not offer `project.observe` (build
the baseline from the commit before the capability). Everything else is held fixed per (task, model):
the project, the goal, the model, the provider, the runtime and the execution limits. Each run is a
fresh copy of the fixture.

Recorded per run: model calls, executions (by capability), provider-reported tokens, the bytes of
project.observe observations the model was shown, wall time, Chip's verified / grounded / goal
fields and useful-work metrics, the safety audit, and, separately and labelled as the harness's own
check, whether the answer or the change was right. For inspect tasks Chip never reports `verified`, so
useful work is 0 in both arms by construction; correctness there is the harness's oracle, not Chip's.

Models need `CHIP_API_KEY` in the environment for Anthropic; nothing here prints or stores it.
"""
import argparse, http.server, json, os, shutil, subprocess, sys, threading, time, urllib.error, urllib.request

# ---- models -------------------------------------------------------------------------------------------
MODELS = {
    "haiku": dict(provider="anthropic", model="claude-haiku-4-5-20251001",
                  upstream="https://api.anthropic.com/v1/messages", path=""),
    "qwen-vllm": dict(provider="openai-compatible", model="mlx-community/Qwen3.5-35B-A3B-4bit",
                      upstream="http://127.0.0.1:8000", path="/v1/chat/completions",
                      env={"CHIP_ENABLE_THINKING": "false"}),
    "qwen-ollama": dict(provider="ollama", model="qwen3-coder:latest",
                        upstream="http://127.0.0.1:11434", path=""),
}
FWD = ("content-type", "x-api-key", "anthropic-version", "anthropic-beta", "authorization")


class Proxy:
    """Records request and response bodies (never headers) between Chip and the model."""

    def __init__(self, upstream, trace):
        self.upstream, self.trace = upstream, trace
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("content-length", 0)))
                headers = {k: v for k, v in self.headers.items() if k.lower() in FWD}
                url = outer.upstream + (self.path if self.path != "/" else "")
                try:
                    with urllib.request.urlopen(urllib.request.Request(url, data=body, headers=headers), timeout=600) as r:
                        out, status = r.read(), r.status
                except urllib.error.HTTPError as e:
                    out, status = e.read(), e.code
                with open(outer.trace, "a") as f:
                    f.write(json.dumps({"status": status, "request": json.loads(body)}) + "\n")
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(out)))
                self.end_headers()
                self.wfile.write(out)

            def log_message(self, *a):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()


# ---- fixtures -----------------------------------------------------------------------------------------
def write(root, path, text):
    full = os.path.join(root, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w") as f:
        f.write(text)


def git_init(root):
    env = dict(os.environ, GIT_AUTHOR_NAME="b", GIT_AUTHOR_EMAIL="b@b", GIT_COMMITTER_NAME="b", GIT_COMMITTER_EMAIL="b@b")
    for cmd in (["init", "-q", "-b", "main"], ["add", "-A"], ["commit", "-q", "-m", "fixture"]):
        subprocess.run(["git", *cmd], cwd=root, env=env, check=True, capture_output=True)


SLICE = ["fx-core", "fx-provider-http", "chip-core", "chip-project", "chip-pax"]


def real_workspace(root, repo):
    """A slice of this repository's own crates: a real, nontrivial Rust workspace."""
    members = []
    for name in SLICE:
        shutil.copytree(os.path.join(repo, "crates", name), os.path.join(root, "crates", name),
                        ignore=shutil.ignore_patterns("target"))
        members.append(f'"crates/{name}"')
    write(root, "Cargo.toml", "[workspace]\nresolver = \"2\"\nmembers = [" + ", ".join(members) + "]\n")
    git_init(root)


def shop(root, bug):
    """A multi-module crate where a test fails because of one small bug in one module."""
    write(root, "Cargo.toml", '[package]\nname = "shop"\nversion = "0.1.0"\nedition = "2021"\n')
    write(root, "src/lib.rs", "pub mod audit;\npub mod catalog;\npub mod customer;\npub mod invoice;\npub mod pricing;\npub mod shipping;\npub mod tax;\npub mod util;\n")
    write(root, "src/util.rs", "pub fn percent_of(amount: u64, bps: u64) -> u64 {\n    amount * bps / 10_000\n}\n\npub fn clamp(value: u64, max: u64) -> u64 {\n    value.min(max)\n}\n")
    write(root, "src/audit.rs", "pub struct AuditEntry {\n    pub action: String,\n}\n\npub fn record(action: &str) -> AuditEntry {\n    AuditEntry { action: action.to_string() }\n}\n")
    write(root, "src/catalog.rs", "pub struct Item {\n    pub sku: &'static str,\n    pub unit_cents: u64,\n}\n\npub fn lookup(sku: &str) -> Option<Item> {\n    match sku {\n        \"pen\" => Some(Item { sku: \"pen\", unit_cents: 200 }),\n        \"book\" => Some(Item { sku: \"book\", unit_cents: 1500 }),\n        _ => None,\n    }\n}\n")
    write(root, "src/customer.rs", "pub struct Customer {\n    pub region: &'static str,\n}\n\npub fn is_business(name: &str) -> bool {\n    name.ends_with(\"Ltd\")\n}\n")
    write(root, "src/shipping.rs", "pub fn flat_cents(region: &str) -> u64 {\n    match region {\n        \"EU\" => 900,\n        _ => 500,\n    }\n}\n")
    write(root, "src/pricing/mod.rs", "pub mod discount;\npub mod tiers;\n\npub fn line_cents(unit: u64, qty: u32) -> u64 {\n    discount::bulk(qty, unit * qty as u64)\n}\n")
    write(root, "src/pricing/tiers.rs", "pub enum Tier {\n    Standard,\n    Bulk,\n}\n\npub fn tier_for(qty: u32) -> Tier {\n    if qty >= 10 { Tier::Bulk } else { Tier::Standard }\n}\n")
    threshold = ">=" if bug != "discount" else ">"
    write(root, "src/pricing/discount.rs", f"use crate::util::percent_of;\n\n/// Orders of ten or more units get ten percent off the line.\npub fn bulk(qty: u32, line: u64) -> u64 {{\n    if qty {threshold} 10 {{\n        line - percent_of(line, 1_000)\n    }} else {{\n        line\n    }}\n}}\n")
    eu = 2000 if bug != "tax" else 1900
    write(root, "src/tax.rs", f"use crate::util::percent_of;\n\n/// Tax in basis points for a shipping region.\npub fn rate_bps(region: &str) -> u64 {{\n    match region {{\n        \"EU\" => {eu},\n        \"US\" => 700,\n        _ => 0,\n    }}\n}}\n\npub fn tax_cents(amount: u64, region: &str) -> u64 {{\n    percent_of(amount, rate_bps(region))\n}}\n")
    write(root, "src/invoice.rs", "use crate::{pricing, shipping, tax};\n\npub struct Order {\n    pub sku_unit_cents: u64,\n    pub qty: u32,\n    pub region: &'static str,\n}\n\n/// Total in cents: discounted line, plus shipping, plus tax on both.\npub fn total_cents(order: &Order) -> u64 {\n    let line = pricing::line_cents(order.sku_unit_cents, order.qty);\n    let before_tax = line + shipping::flat_cents(order.region);\n    before_tax + tax::tax_cents(before_tax, order.region)\n}\n")
    write(root, "tests/totals.rs", "use shop::invoice::{total_cents, Order};\n\n#[test]\nfn an_eu_order_is_taxed_at_twenty_percent() {\n    let order = Order { sku_unit_cents: 1000, qty: 1, region: \"EU\" };\n    // (1000 + 900) * 1.20\n    assert_eq!(total_cents(&order), 2280);\n}\n\n#[test]\nfn ten_units_get_the_bulk_discount() {\n    let order = Order { sku_unit_cents: 1000, qty: 10, region: \"US\" };\n    // (10_000 - 10%) = 9_000, + 500 shipping = 9_500, * 1.07\n    assert_eq!(total_cents(&order), 10_165);\n}\n")
    git_init(root)


# ---- tasks --------------------------------------------------------------------------------------------
TASKS = {
    "I1": dict(kind="inspect", fixture=lambda r, repo: real_workspace(r, repo),
               goal="Which source file declares the function host_path_leak_invariant? Cite the file.",
               expect=["crates/chip-project/src/lib.rs"]),
    "I2": dict(kind="inspect", fixture=lambda r, repo: real_workspace(r, repo),
               goal="Which source file declares the enum WorkOutcome in the chip-core crate? Cite the file.",
               expect=["crates/chip-core/src/work.rs"]),
    "I3": dict(kind="inspect", fixture=lambda r, repo: real_workspace(r, repo),
               goal="Name two test functions in the fx-core crate and cite the file that declares them.",
               expect=["crates/fx-core/tests/provider_boundary.rs"]),
    "I4": dict(kind="inspect", fixture=lambda r, repo: real_workspace(r, repo),
               goal="Which source file in the fx-provider-http crate declares HttpProviderConfig, and what other source files does that crate contain under src? Cite the files.",
               expect=["crates/fx-provider-http/src/lib.rs", "crates/fx-provider-http/src/anthropic.rs", "crates/fx-provider-http/src/ollama.rs", "crates/fx-provider-http/src/openai_compatible.rs"]),
    # A plumbing check, not part of the comparison: the goal tells the model to use the capability.
    "X1": dict(kind="inspect", fixture=lambda r, repo: real_workspace(r, repo),
               goal="Use the project.observe capability with scope crate:fx-core, then say which source file declares the struct ModelRequest. Cite the file.",
               expect=["crates/fx-core/src/lib.rs"]),
    "B1": dict(kind="change", fixture=lambda r, repo: shop(r, "tax"),
               goal="Orders shipped to the EU produce the wrong invoice total. Make the project's tests pass.", expect=[]),
    "B2": dict(kind="change", fixture=lambda r, repo: shop(r, "discount"),
               goal="A bulk order of exactly ten units is not getting the bulk discount. Make the project's tests pass.", expect=[]),
}


# ---- measurement ----------------------------------------------------------------------------------------
def observe_bytes_and_states(trace):
    """What the model was shown from project.observe, from the last request that carried it."""
    last = None
    try:
        for line in open(trace):
            last = json.loads(line)["request"]
    except FileNotFoundError:
        return 0, []
    if not last:
        return 0, []
    texts = []

    def walk(x):
        if isinstance(x, str):
            texts.append(x)
        elif isinstance(x, list):
            for i in x:
                walk(i)
        elif isinstance(x, dict):
            for v in x.values():
                walk(v)

    walk(last.get("messages", []))
    walk(last.get("system", []))  # Anthropic carries observations here
    # Only observation messages: the capability list in the prompt also names project.observe.
    shown = [t for t in texts if t.startswith("Observation:") and 'capability\\":\\"project.observe' in t]
    states = []
    for t in shown:
        marker = 'state\\":\\"'
        at = t.find(marker)
        if at >= 0:
            states.append(t[at + len(marker):].split('\\"')[0])
    return sum(len(t) for t in shown), states


def run_one(binary, task, model, out, repo, tag):
    cfg = MODELS[model]
    spec = TASKS[task]
    root = os.path.join(out, tag, "project")
    shutil.rmtree(os.path.join(out, tag), ignore_errors=True)
    os.makedirs(root)
    spec["fixture"](root, repo)
    trace = os.path.join(out, tag, "trace.jsonl")
    proxy = Proxy(cfg["upstream"], trace)
    env = dict(os.environ, **cfg.get("env", {}))
    if "treatment" in tag:
        # project.observe is explicit: the treatment arm asks for it, the baseline never does.
        env["CHIP_ENABLE_PROJECT_OBSERVE"] = "true"
    else:
        env.pop("CHIP_ENABLE_PROJECT_OBSERVE", None)
    cmd = [binary, "work", "--kind", spec["kind"], "--provider", cfg["provider"], "--model", cfg["model"],
           "--endpoint", f"http://127.0.0.1:{proxy.port}{cfg['path']}", "--json", spec["goal"]]
    start = time.time()
    p = subprocess.run(cmd, cwd=root, env=env, capture_output=True, text=True, timeout=900)
    wall = time.time() - start
    proxy.close()
    try:
        j = json.loads(p.stdout)
    except Exception:
        j = None
    row = dict(task=task, model=model, tag=tag, exit=p.returncode, wall_s=round(wall, 1))
    if j is None:
        row.update(ran=False, stderr=p.stderr[-200:])
        return row
    c = j["context"]
    by = j["executions_by_capability"]
    obytes, states = observe_bytes_and_states(trace)
    answer = j.get("answer") or ""
    if spec["kind"] == "inspect":
        correct = bool(answer) and all(e in answer for e in spec["expect"][:1]) and (
            task != "I4" or all(e in answer for e in spec["expect"]))
    else:
        correct = bool(j["verified"])
    row.update(
        ran=True, terminal=j["terminal_state"], reason=(j.get("outcome_reason") or "")[:90],
        model_calls=j["measurement"]["model_calls"], executions=j["measurement"]["executions"],
        observe_calls=by.get("project.observe", 0), by_capability=by,
        tokens=c["total_reported_tokens"], request_bytes=c["total_request_bytes"],
        observe_bytes_shown=obytes, observe_states=states,
        verified=j["verified"], grounded=j.get("grounded"), goal_satisfied=j["goal_satisfied"],
        useful_per_call=j["useful_work_per_model_call"], useful_per_exec=j["useful_work_per_execution"],
        audit_clean=j["audit"]["clean"], false_completions=j["audit"]["false_completions"],
        unauthorized=j["audit"]["unauthorized_executions"] + j["audit"]["unauthorized_completions"],
        containment=j["audit"]["path_escape"] + j["audit"]["host_path_leak"] + j["audit"]["out_of_root_write"],
        invalid_decisions=j["invalid_decisions"], answer_correct_by_harness=correct,
    )
    return row


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--baseline", required=True)
    ap.add_argument("--treatment", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--repo", default=os.getcwd())
    ap.add_argument("--models", default="haiku,qwen-vllm,qwen-ollama")
    ap.add_argument("--tasks", default="all")
    ap.add_argument("--repeat", type=int, default=1)
    a = ap.parse_args()
    tasks = [t for t in TASKS if not t.startswith("X")] if a.tasks == "all" else a.tasks.split(",")
    os.makedirs(a.out, exist_ok=True)
    rows = []
    for model in a.models.split(","):
        for task in tasks:
            for rep in range(a.repeat):
                for arm, binary in (("baseline", a.baseline), ("treatment", a.treatment)):
                    tag = f"{model}-{task}-{arm}-{rep}"
                    row = run_one(binary, task, model, a.out, a.repo, tag)
                    row["arm"], row["rep"] = arm, rep
                    rows.append(row)
                    print(json.dumps({k: row.get(k) for k in ("model", "task", "arm", "terminal", "model_calls", "executions", "observe_calls", "tokens", "verified", "answer_correct_by_harness", "wall_s", "reason")}), flush=True)
                    with open(os.path.join(a.out, "results.json"), "w") as f:
                        json.dump(rows, f, indent=1)


if __name__ == "__main__":
    sys.exit(main())
