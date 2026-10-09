"""A *real* PAX and cargo, with a test that outlives Chip's 300 s limit: are the grandchildren reaped?"""
import json, os, subprocess, sys, time
sys.path.insert(0, os.path.dirname(__file__))
from runner import *
OUT = os.environ.get("AUDIT_OUT", "/home/user/chip-rs/docs/product/audit-evidence/hostile-audit")
def prep(proj):
    open(os.path.join(proj, "tests/slow.rs"), "w").write("#[test]\nfn slow() { std::thread::sleep(std::time::Duration::from_secs(420)); }\n")
rec = run_work("o2-real-pax-hung-test", "Verify the project", lambda n, t, r: call("pax.test") if n == 0 else complete("x"), kind="verify", defects=(), prepare=prep, timeout=600)
alive = subprocess.run(["pgrep", "-fc", "slow-"], capture_output=True, text=True).stdout.strip()
cargo = subprocess.run(["pgrep", "-fc", "cargo test"], capture_output=True, text=True).stdout.strip()
c = rec["chip"]
out = {"chip_terminal_state": c.get("terminal_state"), "exit": rec["exit"], "wall_s": rec["wall_s"], "outcome_reason": c.get("outcome_reason"),
       "test_binary_processes_still_alive_after_chip_exited": alive, "cargo_test_processes_still_alive": cargo}
subprocess.run(["pkill", "-f", "slow-"]); subprocess.run(["pkill", "-f", "cargo test"])
json.dump(out, open(os.path.join(OUT, "summary-hung-real-pax.json"), "w"), indent=1)
print(out)
