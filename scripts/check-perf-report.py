#!/usr/bin/env python3
"""Checks a `chip.perf-baseline.v2` report against the schema in docs/product/performance.md.

    python3 scripts/check-perf-report.py target/chip-baseline.json [--require-fx] [--live]

Checks structure and types only. It never compares a measured value with a threshold: timing is
environment-dependent and no number here is a budget. Exit status 0 when the report conforms.
"""
import json
import sys

ROW_KEYS = {"id", "tier", "class", "name", "n", "unit", "min", "p50", "p95", "max",
            "throughput", "throughput_unit", "note"}
PROVENANCE = {"schema_version", "timestamp_utc", "git_sha", "git_dirty", "rust_version",
              "build_profile", "os", "os_release", "arch", "cpu_model", "cpu_count",
              "runtime_worker_threads", "sample_mode", "sections"}
CLASSES = {"direct-microbenchmark", "process-level", "real-model"}
BOUNDARIES = ["b1_submit_to_loop_start", "b2_loop_before_inference", "b3_fx_invocation_to_dispatch",
              "b4_dispatch_to_first_event", "b5_first_event_to_terminal", "b6_loop_after_inference",
              "b7_tool_execution", "b8_total"]
FX_SCENARIOS = ["fx.turn.no_tool", "fx.turn.tool_round_trip", "fx.turn.steps.10", "fx.turn.steps.50",
                "fx.concurrent.1", "fx.concurrent.4", "fx.concurrent.16"]


def fail(message):
    print(f"FAIL: {message}")
    sys.exit(1)


def num(x):
    return isinstance(x, (int, float)) and not isinstance(x, bool)


def main(argv):
    paths = [a for a in argv if not a.startswith("--")]
    if len(paths) != 1:
        fail("usage: check-perf-report.py REPORT.json [--require-fx] [--live]")
    try:
        with open(paths[0]) as f:
            doc = json.load(f)
    except (OSError, ValueError) as e:
        fail(f"cannot read {paths[0]} as JSON: {e}")

    if doc.get("schema") != "chip.perf-baseline.v2":
        fail(f"schema is {doc.get('schema')!r}, expected 'chip.perf-baseline.v2'")
    for key in ("provenance", "facts", "skipped", "scenarios", "rows", "unavailable", "errors", "real_model"):
        if key not in doc:
            fail(f"missing top-level key {key!r}")

    prov = doc["provenance"]
    if set(prov) != PROVENANCE:
        fail(f"provenance keys differ: missing {sorted(PROVENANCE - set(prov))}, extra {sorted(set(prov) - PROVENANCE)}")
    if prov["schema_version"] != doc["schema"]:
        fail("provenance.schema_version does not match schema")
    ts = prov["timestamp_utc"]
    if not (isinstance(ts, str) and len(ts) == 20 and ts.endswith("Z") and ts[10] == "T"):
        fail(f"timestamp_utc is not YYYY-MM-DDTHH:MM:SSZ: {ts!r}")
    if prov["git_dirty"] is not None and not isinstance(prov["git_dirty"], bool):
        fail("git_dirty must be true, false or null (null = could not be determined)")
    for key in ("git_sha", "rust_version", "os_release", "cpu_model"):
        if not isinstance(prov[key], str) or not prov[key]:
            fail(f"provenance.{key} must be a non-empty string ('unavailable' when unknown)")

    ids = set()
    for row in doc["rows"]:
        if set(row) != ROW_KEYS:
            fail(f"row {row.get('id')!r} keys differ: {sorted(set(row) ^ ROW_KEYS)}")
        if row["id"] in ids:
            fail(f"duplicate row id {row['id']!r}")
        ids.add(row["id"])
        if row["class"] not in CLASSES:
            fail(f"row {row['id']!r}: unknown class {row['class']!r}")
        if not (isinstance(row["n"], int) and row["n"] >= 1):
            fail(f"row {row['id']!r}: n must be a positive integer")
        for key in ("min", "p50", "max"):
            if not num(row[key]):
                fail(f"row {row['id']!r}: {key} must be a number")
        if not row["min"] <= row["p50"] <= row["max"]:
            fail(f"row {row['id']!r}: min <= p50 <= max does not hold")
        if row["n"] < 20 and row["p95"] is not None:
            fail(f"row {row['id']!r}: p95 reported with only {row['n']} samples (must be null)")
        if row["n"] >= 20 and not num(row["p95"]):
            fail(f"row {row['id']!r}: p95 must be a number with {row['n']} samples")
        if (row["throughput"] is None) != (row["throughput_unit"] is None):
            fail(f"row {row['id']!r}: throughput and throughput_unit must be given together")

    scenario_ids = [s["id"] for s in doc["scenarios"]]
    if len(set(scenario_ids)) != len(scenario_ids):
        fail("duplicate scenario id")
    for s in doc["scenarios"]:
        if "kind" not in s["config"]:
            fail(f"scenario {s['id']!r} has no config.kind")
        if s["config"]["kind"] != "real-model" and "iterations" not in s["config"]:
            fail(f"scenario {s['id']!r} has no iteration count")
    for u in doc["unavailable"]:
        if not (isinstance(u.get("id"), str) and isinstance(u.get("reason"), str) and u["reason"]):
            fail(f"unavailable entries need an id and a reason: {u!r}")
    for e in doc["errors"]:
        if not (isinstance(e.get("id"), str) and isinstance(e.get("error"), str)):
            fail(f"error entries need an id and an error: {e!r}")

    if "--require-fx" in argv:
        for sid in FX_SCENARIOS:
            if sid not in scenario_ids:
                fail(f"missing scenario {sid!r}")
        unavailable = {u["id"] for u in doc["unavailable"]}
        for sid in FX_SCENARIOS[:4]:
            for b in BOUNDARIES:
                if f"{sid}.{b}" not in ids and f"{sid}.{b}" not in unavailable:
                    fail(f"{sid}.{b} is neither measured nor reported unavailable")
        for w in (1, 4, 16):
            if f"fx.concurrent.{w}.batch_wall" not in ids:
                fail(f"missing fx.concurrent.{w}.batch_wall")
        if "fx.agent_loop_start" not in unavailable:
            fail("fx.agent_loop_start must be reported unavailable")
        if doc["real_model"] is not None and doc["real_model"] != {} and any(r["class"] == "real-model" for r in doc["rows"]):
            fail("a deterministic report must not contain real-model rows")

    if "--live" in argv:
        rm = doc["real_model"]
        if not isinstance(rm, dict) or "label" not in rm:
            fail("a live report needs a real_model object with a label")
        if rm.get("status") == "run":
            for key in ("model", "endpoint", "endpoint_type", "credential_supplied", "config", "iterations",
                        "requests_made", "time_to_first_response_event"):
                if key not in rm:
                    fail(f"real_model.{key} is missing (use null when unavailable)")
            if rm["requests_made"] > rm["config"]["iterations"]:
                fail("more requests than iterations")
        if any(r["class"] != "real-model" for r in doc["rows"]):
            fail("a live report must not contain synthetic rows")

    print(f"OK: {paths[0]}: {len(doc['rows'])} rows, {len(scenario_ids)} scenarios, "
          f"{len(doc['unavailable'])} unavailable, {len(doc['errors'])} errors")


main(sys.argv[1:])
