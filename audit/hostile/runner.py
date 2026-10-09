import json, os, shutil, subprocess, sys, time, signal, tempfile
sys.path.insert(0, os.path.dirname(__file__))
import fixture, mockmodel
from mockmodel import Mock, call, complete, escalate, block

ROOT = os.environ.get("AUDIT_ROOT", "/tmp/claude-0/aud")
CHIP = os.environ.get("CHIP_BIN", "/home/user/audit-clean/target/release/chip")
PAX = os.environ.get("PAX_BIN", "/home/user/rkendel1/pax/target/release/pax")
SHARED_TARGET = os.path.join(ROOT, "shared-target")
os.makedirs(ROOT, exist_ok=True)


def fixed_text(rel):
    d = tempfile.mkdtemp(prefix="fx-")
    fixture.make(d, defects=())
    s = open(os.path.join(d, rel)).read()
    shutil.rmtree(d, ignore_errors=True)
    return s


def only_a_fixed(rel):
    d = tempfile.mkdtemp(prefix="fx-")
    fixture.make(d, defects=("B",))
    s = open(os.path.join(d, rel)).read()
    shutil.rmtree(d, ignore_errors=True)
    return s


def base_env(mock_url, extra=None):
    env = dict(os.environ)
    env.update({
        "CHIP_PROVIDER": "openai-compatible", "CHIP_MODEL": "audit-adversary",
        "CHIP_ENDPOINT": mock_url, "CHIP_API_KEY": "sk-audit-not-a-secret",
        "PAX_BIN": PAX, "CARGO_TARGET_DIR": SHARED_TARGET,
    })
    if extra:
        env.update(extra)
    return env


def diff_summary(proj):
    r = subprocess.run(["git", "status", "--porcelain"], cwd=proj, capture_output=True, text=True)
    return [l for l in r.stdout.splitlines()]


def run_work(name, goal, script, kind=None, defects=("A", "B"), chip_args=(), timeout=300, prepare=None, env_extra=None, delay=0.0, feature=False, ctx=None):
    """One `chip work` against a fresh fixture and a scripted adversary. Returns a record."""
    run_dir = os.path.join(ROOT, "runs", name)
    shutil.rmtree(run_dir, ignore_errors=True)
    proj = os.path.join(run_dir, "proj")
    fixture.make(proj, defects=defects)
    if ctx is not None:
        ctx['proj'] = proj
    if prepare:
        prepare(proj)
    mock = Mock(script, delay=delay)
    args = [CHIP, "work", goal, "--json"] + (["--kind", kind] if kind else []) + list(chip_args)
    t0 = time.time()
    p = subprocess.run(args, cwd=proj, env=base_env(mock.url, env_extra), capture_output=True, text=True, timeout=timeout)
    wall = time.time() - t0
    rec = {
        "name": name, "goal": goal, "kind": kind, "exit": p.returncode, "wall_s": round(wall, 2),
        "model_requests": len(mock.requests),
        "request_bytes": [r["bytes"] for r in mock.requests],
        "stderr_tail": p.stderr[-400:],
    }
    try:
        rec["chip"] = json.loads(p.stdout)
    except Exception:
        rec["stdout_raw"] = p.stdout[-1500:]
    rec["changed_files"] = diff_summary(proj)
    rec["acceptance"] = fixture.accept(proj, SHARED_TARGET, feature=feature)
    mock.close()
    rec["_mock_requests"] = mock.requests
    rec["_proj"] = proj
    return rec


def save(rec, path):
    r = {k: v for k, v in rec.items() if not k.startswith("_")}
    os.makedirs(os.path.dirname(path), exist_ok=True)
    json.dump(r, open(path, "w"), indent=1)


def summarize(rec):
    c = rec.get("chip", {})
    return {
        "name": rec["name"], "exit": rec["exit"], "outcome": c.get("outcome"), "lifecycle": c.get("lifecycle") or c.get("terminal_state"),
        "verified": c.get("verified"), "goal_satisfied": c.get("goal_satisfied"), "model_requests": rec["model_requests"],
        "executions": c.get("executions_by_capability"), "acceptance_visible": rec["acceptance"]["visible_pass"],
        "acceptance_hidden": rec["acceptance"]["hidden_pass"], "changed": rec["changed_files"], "wall_s": rec["wall_s"],
    }


if __name__ == "__main__":
    seq = [
        call("project.write", path="src/money.rs", content=fixed_text("src/money.rs")),
        call("project.write", path="src/account.rs", content=fixed_text("src/account.rs")),
        call("pax.test"),
    ]
    rec = run_work("s00-control", "Fix the failing tests in this project", lambda n, t, r: seq[n] if n < len(seq) else complete("done"))
    print(json.dumps(summarize(rec), indent=1))
    print(sorted(rec["chip"].keys()))
