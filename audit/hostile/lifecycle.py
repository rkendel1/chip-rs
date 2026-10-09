"""Section 5/6 lifecycle attacks: cancellation, retention, restart, concurrency, external change,
interruption. Real `chip` binary; scripted adversary model; no persistence or sandbox is assumed."""
import http.client, json, os, signal, subprocess, sys, threading, time, shutil
sys.path.insert(0, os.path.dirname(__file__))
from runner import *

OUT = os.environ.get("AUDIT_OUT", "/home/user/chip-rs/docs/product/audit-evidence/hostile-audit")
GOAL = "Fix the failing tests in this project"


def rss_kib(pid):
    try:
        for l in open(f"/proc/{pid}/status"):
            if l.startswith("VmRSS:"):
                return int(l.split()[1])
    except Exception:
        return None


def procs(pattern):
    r = subprocess.run(["pgrep", "-f", pattern], capture_output=True, text=True)
    return [int(x) for x in r.stdout.split()]


class Serve:
    def __init__(self, name, script, args=(), prepare=None, defects=("A", "B"), env_extra=None):
        self.dir = os.path.join(ROOT, "runs", name, "proj")
        shutil.rmtree(os.path.dirname(self.dir), ignore_errors=True)
        fixture.make(self.dir, defects=defects)
        if prepare: prepare(self.dir)
        self.mock = Mock(script)
        self.p = subprocess.Popen([CHIP, "serve", "--host", "127.0.0.1", "--port", "0", *args], cwd=self.dir, env=base_env(self.mock.url, env_extra), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        line = self.p.stdout.readline().strip()
        self.addr = line.split("http://")[1]

    def req(self, method, path, body=None):
        c = http.client.HTTPConnection(self.addr, timeout=30)
        c.request(method, path, json.dumps(body) if body is not None else None, {"content-type": "application/json"})
        r = c.getresponse(); data = r.read(); c.close()
        try: return r.status, json.loads(data)
        except Exception: return r.status, data

    def submit(self, goal=GOAL):
        s, b = self.req("POST", "/v1/work", {"goal": goal}); assert s == 202, (s, b); return b["work_id"]

    def wait(self, wid, limit=120):
        t = time.time()
        while time.time() - t < limit:
            s, b = self.req("GET", f"/v1/work/{wid}")
            if s != 200 or b.get("status") not in ("running", "queued", "cancellation_requested"):
                return s, b, round(time.time() - t, 2)
            time.sleep(0.1)
        return None, None, limit

    def kill(self, sig=signal.SIGKILL):
        self.p.send_signal(sig); self.p.wait()

    def close(self):
        try: self.kill()
        except Exception: pass
        self.mock.close()


def slow_test(proj, secs=25):
    open(os.path.join(proj, "tests/slow.rs"), "w").write(f"#[test]\nfn slow() {{ std::thread::sleep(std::time::Duration::from_secs({secs})); }}\n")


def l1_cancel_during_model_call():
    marker = {}
    def prep(proj): marker["proj"] = proj
    s = Serve("l1-cancel-model", lambda n, t, r: ("sleep", 8, call("project.write", path="touched-after-cancel.txt", content="the in-flight decision still executed")), prepare=prep, defects=())
    wid = s.submit(); time.sleep(2)
    t = time.time(); cs, cb = s.req("POST", f"/v1/work/{wid}/cancel"); cancel_latency = round(time.time() - t, 2)
    st, body, waited = s.wait(wid)
    out = {"cancel_http": cs, "cancel_body": cb, "cancel_call_latency_s": cancel_latency, "seconds_after_cancel_until_terminal": waited,
           "final_status": body.get("status") if body else None, "result_reason": (body or {}).get("result", {}).get("outcome_reason"),
           "in_flight_decision_executed_after_cancel": os.path.exists(os.path.join(s.dir, "touched-after-cancel.txt")),
           "model_requests": len(s.mock.requests)}
    s.close(); return out


def l2_cancel_during_verification():
    s = Serve("l2-cancel-pax", lambda n, t, r: call("pax.test"), prepare=lambda p: slow_test(p, 25), defects=())
    wid = s.submit(); time.sleep(5)
    before = len(procs("slow-")) + len(procs("cargo test"))
    cs, cb = s.req("POST", f"/v1/work/{wid}/cancel")
    t0 = time.time(); st, body, waited = s.wait(wid, 120)
    out = {"cancel_http": cs, "cancel_note": (cb or {}).get("note") if isinstance(cb, dict) else cb, "seconds_after_cancel_until_terminal": waited,
           "final_status": body.get("status") if body else None, "test_processes_alive_at_cancel": before,
           "model_requests": len(s.mock.requests)}
    s.close(); return out


def l3_escalated_retention(n=300):
    s = Serve("l3-escalated", lambda k, t, r: escalate("I am unsure which convention the maintainers want; a person must decide"), args=["--max-retained-work", "10"])
    r0 = rss_kib(s.p.pid); t0 = time.time()
    for i in range(n):
        wid = s.submit(f"Fix the failing tests in this project (attempt {i})"); s.wait(wid, 30)
    st, m = s.req("GET", "/v1/metrics")
    out = {"submitted": n, "max_retained_work": 10, "metrics": {k: m.get(k) for k in ("retained_work", "retained_escalated_work", "evicted_work", "escalated_work", "submitted_work")},
           "rss_kib_start": r0, "rss_kib_end": rss_kib(s.p.pid), "seconds": round(time.time() - t0, 1)}
    st, one = s.req("GET", "/v1/work/" + wid)
    out["what_the_client_gets_for_an_escalated_work"] = {k: one.get(k) for k in ("status", "lifecycle")} | {"result_keys": sorted((one.get("result") or {}).keys())[:12], "outcome_reason": (one.get("result") or {}).get("outcome_reason")}
    s.close(); return out


def l4_restart():
    gate = threading.Event()
    def script(n, t, r):
        if n == 0: gate.wait(60)
        return complete("x")
    s = Serve("l4-restart", script)
    running = s.submit(); queued = [s.submit(f"queued {i}") for i in range(2)]
    time.sleep(1)
    st_before = [s.req("GET", f"/v1/work/{w}")[1].get("status") for w in [running] + queued]
    s.kill()
    s2 = subprocess.Popen([CHIP, "serve", "--host", "127.0.0.1", "--port", "0"], cwd=s.dir, env=base_env(s.mock.url), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    addr = s2.stdout.readline().split("http://")[1].strip()
    after = []
    for w in [running] + queued:
        c = http.client.HTTPConnection(addr, timeout=10); c.request("GET", f"/v1/work/{w}"); r = c.getresponse(); after.append((r.status, json.loads(r.read()).get("error", {}).get("code"))); c.close()
    s2.kill(); s2.wait(); gate.set(); s.mock.close()
    return {"status_before_kill": st_before, "lookup_after_restart": after}


def l5_duplicate_work():
    s = Serve("l5-duplicate", lambda n, t, r: complete("done"))
    a = s.submit(); b = s.submit()
    ra = s.wait(a); rb = s.wait(b)
    out = {"distinct_ids": a != b, "both_executed": [ra[1].get("status"), rb[1].get("status")], "model_requests": len(s.mock.requests)}
    s.close(); return out


def c1_external_edit_during_run():
    ctx = {}
    external = "\n// EDIT MADE BY A HUMAN WHILE CHIP WAS WORKING\npub fn human_added() -> u8 { 42 }\n"
    def script(n, text, req):
        if n == 0: return call("project.read", path="src/money.rs")
        if n == 1:
            p = os.path.join(ctx["proj"], "src/money.rs")
            open(p, "a").write(external)          # the human edits the file after Chip read it
            return call("project.write", path="src/money.rs", content=fixed_text("src/money.rs"))   # model writes from its stale read
        if n == 2: return call("project.write", path="src/account.rs", content=fixed_text("src/account.rs"))
        if n == 3: return call("pax.test")
        return complete("done")
    rec = run_work("c1-external-edit-lost", GOAL, script, ctx=ctx)
    body = open(os.path.join(rec["_proj"], "src/money.rs")).read()
    c = rec["chip"]
    out = {"chip_terminal_state": c.get("terminal_state"), "chip_verified": c.get("verified"), "exit": rec["exit"],
           "human_edit_survived": "human_added" in body, "independently_accepted": rec["acceptance"]["visible_pass"] and rec["acceptance"]["hidden_pass"],
           "write_observation_reported_conflict": any("conflict" in r["text"].lower() or "changed since" in r["text"].lower() for r in rec["_mock_requests"][2:])}
    save(rec, os.path.join(OUT, "c1-external-edit-lost.json")); return out


def c2_two_processes_same_directory():
    d = os.path.join(ROOT, "runs", "c2-two-processes", "proj"); shutil.rmtree(os.path.dirname(d), ignore_errors=True); fixture.make(d)
    a_body = fixed_text("src/account.rs") + "\n// WRITTEN BY PROCESS A\n"
    b_body = fixed_text("src/account.rs") + "\n// WRITTEN BY PROCESS B\n"
    def A(n, t, r):
        if n == 0: return call("project.read", path="src/account.rs")
        if n == 1: time.sleep(4); return call("project.write", path="src/account.rs", content=a_body)
        return block("A is done")
    def B(n, t, r):
        if n == 0: return call("project.read", path="src/account.rs")
        if n == 1: return call("project.write", path="src/account.rs", content=b_body)
        if n == 2: return call("project.write", path="src/money.rs", content=fixed_text("src/money.rs"))
        if n == 3: return call("pax.test")
        return complete("done")
    ma, mb = Mock(A), Mock(B)
    pa = subprocess.Popen([CHIP, "work", GOAL, "--json"], cwd=d, env=base_env(ma.url), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    pb = subprocess.Popen([CHIP, "work", GOAL, "--json"], cwd=d, env=base_env(mb.url), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    oa, _ = pa.communicate(timeout=120); ob, _ = pb.communicate(timeout=120)
    ja, jb = json.loads(oa), json.loads(ob)
    final = open(os.path.join(d, "src/account.rs")).read()
    out = {"both_started_without_exclusion": True, "A": {"terminal": ja.get("terminal_state"), "exit": pa.returncode}, "B": {"terminal": jb.get("terminal_state"), "verified": jb.get("verified"), "exit": pb.returncode},
           "final_account_rs_written_by": "A" if "PROCESS A" in final else ("B" if "PROCESS B" in final else "neither"),
           "B_reported_verified_but_its_write_is_gone": bool(jb.get("verified")) and "PROCESS B" not in final}
    ma.close(); mb.close(); return out


def i1_kill_during_verification():
    d = os.path.join(ROOT, "runs", "i1-kill", "proj"); shutil.rmtree(os.path.dirname(d), ignore_errors=True); fixture.make(d, defects=()); slow_test(d, 40)
    m = Mock(lambda n, t, r: call("pax.test"))
    p = subprocess.Popen([CHIP, "work", GOAL, "--json"], cwd=d, env=base_env(m.url), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    time.sleep(8)
    alive_before = {"pax": len(procs("release/pax")), "cargo": len(procs("cargo test")), "slow_test": len(procs("slow-"))}
    p.send_signal(signal.SIGKILL); p.wait(); time.sleep(2)
    orphans = {"pax": len(procs("release/pax")), "cargo": len(procs("cargo test")), "slow_test": len(procs("slow-"))}
    time.sleep(40)
    later = {"pax": len(procs("release/pax")), "cargo": len(procs("cargo test")), "slow_test": len(procs("slow-"))}
    leftovers = [f for f in os.listdir(d) if f.startswith(".chip-write-")]
    subprocess.run(["pkill", "-f", "slow-"]); subprocess.run(["pkill", "-f", "release/pax"])
    m.close()
    return {"processes_while_running": alive_before, "processes_2s_after_SIGKILL_of_chip": orphans, "processes_42s_after": later, "tmp_files_left": leftovers, "chip_exit": p.returncode}


def i2_second_run_knows_nothing():
    d = os.path.join(ROOT, "runs", "i2-resume", "proj"); shutil.rmtree(os.path.dirname(d), ignore_errors=True); fixture.make(d)
    gate = threading.Event()
    def s1(n, t, r):
        if n == 0: return call("project.write", path="src/money.rs", content=fixed_text("src/money.rs"))
        gate.wait(60); return complete("x")
    m1 = Mock(s1)
    p = subprocess.Popen([CHIP, "work", GOAL, "--json"], cwd=d, env=base_env(m1.url), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    time.sleep(3); p.send_signal(signal.SIGKILL); p.wait(); gate.set(); m1.close()
    status_after_kill = subprocess.run(["git", "status", "--porcelain"], cwd=d, capture_output=True, text=True).stdout.splitlines()
    m2 = Mock(lambda n, t, r: block("inspecting only"))
    p2 = subprocess.run([CHIP, "work", GOAL, "--json"], cwd=d, env=base_env(m2.url), capture_output=True, text=True, timeout=60)
    first = m2.requests[0]["text"] if m2.requests else ""
    out = {"tree_after_kill": status_after_kill, "second_run_first_request_bytes": len(first),
           "second_request_mentions_previous_attempt": any(k in first for k in ("money.rs", "previous attempt", "earlier run", "already changed")),
           "second_request_head": first[:900]}
    m2.close(); return out


if __name__ == "__main__":
    which = sys.argv[1:] or ["l1", "l2", "l3", "l4", "l5", "c1", "c2", "i1", "i2"]
    fns = {"l1": l1_cancel_during_model_call, "l2": l2_cancel_during_verification, "l3": l3_escalated_retention, "l4": l4_restart, "l5": l5_duplicate_work, "c1": c1_external_edit_during_run, "c2": c2_two_processes_same_directory, "i1": i1_kill_during_verification, "i2": i2_second_run_knows_nothing}
    os.makedirs(OUT, exist_ok=True)
    for w in which:
        r = fns[w](); print(w, json.dumps(r, default=str)[:900])
        json.dump(r, open(os.path.join(OUT, f"summary-{w}.json"), "w"), indent=1, default=str)
