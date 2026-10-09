import json, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from runner import *
OUT = os.environ.get("AUDIT_OUT", "/home/user/chip-rs/docs/product/audit-evidence/hostile-audit")
def mk(where):
    def prep(proj):
        body = 'for i in 0..40000 { println!("noise line {i} lorem ipsum dolor sit amet consectetur"); }\n' if where == "after" else ''
        pre = 'for i in 0..40000 { eprintln!("noise line {i} lorem ipsum dolor sit amet consectetur"); }\n'
        # a failing test whose decisive message is at the START or at the END of ~2 MB of output
        decisive = 'panic!("DECISIVE-FAILURE-MARKER: expected 12.50 got 12.05");\n'
        src = "#[test]\nfn flood() {\n" + (decisive.replace("panic!", "eprintln!") + pre + "assert!(false);\n" if where == "start" else pre + decisive) + "}\n"
        open(os.path.join(proj, "tests/flood.rs"), "w").write(src)
    return prep
out = {}
for where in ("start", "end"):
    rec = run_work("o7-flood-" + where, "Fix the failing tests", lambda n, t, r: call("pax.test") if n == 0 else block("stop"), defects=(), prepare=mk(where))
    second = rec["_mock_requests"][1]["text"] if len(rec["_mock_requests"]) > 1 else ""
    out[where] = {"second_request_bytes": rec["request_bytes"][1] if len(rec["request_bytes"]) > 1 else None, "decisive_marker_reached_model": "DECISIVE-FAILURE-MARKER" in second,
                  "truncation_notice_in_request": [k for k in ("truncated", "omitted", "cap", "bytes omitted") if k in second.lower()], "terminal_state": rec["chip"].get("terminal_state")}
    print(where, out[where])
json.dump(out, open(os.path.join(OUT, "summary-o7-flood.json"), "w"), indent=1)
