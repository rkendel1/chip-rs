//! The host-driven session, played natively. The test is the host: it answers every
//! `need_inference` with the real FX executor (`fx-core` over `fx-single-attempt`) and every
//! `need_execution` with a counting capability. Nothing in the session is faked.

use chip_wasm_host::Session;
use fx_core::{Limits, NoHost, encode_report, execute, parse_request};
use fx_single_attempt::testing::{FakePort, failure};
use fx_single_attempt::{Observed, PortFailureKind, SingleAttemptCapability};
use serde_json::{Value, json};

const REQUEST_SELFTEST: &str =
    r#"{"decision":"request_capability","capability":"compute.selftest"}"#;
const COMPLETE: &str = r#"{"decision":"complete","summary":"the self test passed"}"#;

fn begin_document() -> Value {
    json!({
        "v": 1,
        "work_id": "w",
        "goal": "Run the self test and report the result",
        "model": "model-x",
        "request_seed": "6368697000000000",
        "options_policy": "drop_unsupported",
        "capabilities": [{
            "id": "compute.selftest",
            "name": "selftest",
            "description": "runs the self test",
            "inputs": [{"name": "n", "required": false}]
        }]
    })
}

fn call(session: &mut Session, begin: bool, document: &Value) -> Value {
    let bytes = document.to_string().into_bytes();
    let response = if begin {
        session.begin(&bytes)
    } else {
        session.command(&bytes)
    };
    serde_json::from_slice(&response).expect("every response is JSON")
}

/// What an FX attempt over a scripted port returns, as the JSON report a host would send.
fn fx_report(request: &Value, attempt: FakePort) -> Value {
    let request = parse_request(request.to_string().as_bytes(), &Limits::default())
        .expect("the session builds an FX request that FX itself accepts");
    let mut capability = SingleAttemptCapability::new(attempt);
    let report = execute(&mut capability, &request, &Limits::default(), &mut NoHost);
    serde_json::from_slice(&encode_report(&report)).unwrap()
}

#[test]
fn a_multi_step_work_runs_through_fx_and_the_hosts_capability() {
    let mut session = Session::new();
    let first = call(&mut session, true, &begin_document());
    assert_eq!(first["status"], "need_inference", "{first}");
    assert_eq!(first["request_id"].as_str().unwrap().len(), 32);
    assert!(
        first["request"]["tools"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "no provider tools"
    );

    let second = call(
        &mut session,
        false,
        &json!({"cmd": "inference_result", "report": fx_report(&first["request"], FakePort::text(&[REQUEST_SELFTEST]))}),
    );
    assert_eq!(
        second["status"], "need_execution",
        "the model's request became an execution Chip constructed: {second}"
    );
    assert_eq!(second["execution"]["capability"], "compute.selftest");
    assert!(
        second["execution"]["id"]
            .as_str()
            .unwrap()
            .starts_with("w-exec-"),
        "Chip assigns the id"
    );

    let third = call(
        &mut session,
        false,
        &json!({"cmd": "execution_result", "status": "success", "output": "self test passed", "receipt_id": "sha256:r"}),
    );
    assert_eq!(third["status"], "need_inference", "{third}");
    assert_ne!(
        third["request_id"], first["request_id"],
        "each attempt has its own id"
    );
    let observed = third["request"]["messages"].to_string();
    assert!(
        observed.contains("self test passed"),
        "the result is in the next inference: {observed}"
    );

    let done = call(
        &mut session,
        false,
        &json!({"cmd": "inference_result", "report": fx_report(&third["request"], FakePort::text(&[COMPLETE]))}),
    );
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["result"]["state"], "completed");
    assert_eq!(done["result"]["summary"], "the self test passed");
    assert_eq!(done["result"]["measurement"]["model_calls"], 2);
    assert_eq!(done["result"]["measurement"]["executions"], 1);
    assert_eq!(done["result"]["trajectory_problems"], json!([]));

    let late = call(&mut session, false, &json!({"cmd": "cancel"}));
    assert_eq!(
        late,
        json!({"status": "done", "ignored": true}),
        "nothing is accepted after the end"
    );
}

#[test]
fn an_unauthorized_or_malformed_decision_never_reaches_the_hosts_capability() {
    for reply in [
        r#"{"decision":"request_capability","capability":"deploy.production"}"#,
        "not json",
        r#"{"decision":"request_capability","capability":"compute.selftest","shell":"rm -rf /"}"#,
    ] {
        let mut session = Session::new();
        let first = call(&mut session, true, &begin_document());
        let done = call(
            &mut session,
            false,
            &json!({"cmd": "inference_result", "report": fx_report(&first["request"], FakePort::text(&[reply]))}),
        );
        assert_eq!(done["status"], "done", "{reply}: {done}");
        assert_ne!(done["result"]["state"], "completed", "{reply}");
        assert_eq!(done["result"]["measurement"]["executions"], 0, "{reply}");
        assert_eq!(
            done["result"]["measurement"]["model_calls"], 1,
            "{reply}: no repair call"
        );
    }
}

#[test]
fn an_fx_failure_ends_the_work_with_its_delivery_and_asks_for_no_second_inference() {
    let mut session = Session::new();
    let first = call(&mut session, true, &begin_document());
    let mut port = FakePort::new(Vec::new());
    port.invoke = Err(failure(PortFailureKind::ProviderError, Observed::Invoked));
    let done = call(
        &mut session,
        false,
        &json!({"cmd": "inference_result", "report": fx_report(&first["request"], port)}),
    );
    assert_eq!(
        done["status"], "done",
        "an ambiguous attempt is not repeated: {done}"
    );
    assert_eq!(done["result"]["state"], "failed");
    let reason = done["result"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("possibly_sent") && reason.contains("not retried"),
        "{reason}"
    );
}

#[test]
fn a_host_error_is_a_failed_model_call_not_a_retry() {
    let mut session = Session::new();
    call(&mut session, true, &begin_document());
    let done = call(
        &mut session,
        false,
        &json!({"cmd": "inference_result", "host_error": "the bridge failed"}),
    );
    assert_eq!(done["result"]["state"], "failed", "{done}");
    assert_eq!(done["result"]["measurement"]["model_calls"], 1);
}

#[test]
fn cancel_stops_the_next_model_call_without_asking_fx() {
    let mut session = Session::new();
    let first = call(&mut session, true, &begin_document());
    let again = call(&mut session, false, &json!({"cmd": "cancel"}));
    assert_eq!(
        again, first,
        "cancel is idempotent: it repeats what is pending"
    );
    let second = call(
        &mut session,
        false,
        &json!({"cmd": "inference_result", "report": fx_report(&first["request"], FakePort::text(&[REQUEST_SELFTEST]))}),
    );
    // The work is cancelled, so the capability still runs only if the host chooses; the model
    // is not asked again after the execution.
    assert_eq!(second["status"], "need_execution", "{second}");
    let done = call(
        &mut session,
        false,
        &json!({"cmd": "execution_result", "status": "cancelled", "output": "stopped"}),
    );
    assert_eq!(
        done["status"], "done",
        "no further inference was requested: {done}"
    );
    assert_eq!(
        done["result"]["measurement"]["model_calls"], 2,
        "the second call was refused before FX"
    );
    assert_eq!(done["result"]["state"], "failed");
    assert!(
        done["result"]["reason"]
            .as_str()
            .unwrap()
            .contains("cancellation was requested")
    );
}

#[test]
fn a_capability_failure_is_an_observation_and_the_loop_continues_within_its_limits() {
    let mut session = Session::new();
    let first = call(&mut session, true, &begin_document());
    call(
        &mut session,
        false,
        &json!({"cmd": "inference_result", "report": fx_report(&first["request"], FakePort::text(&[REQUEST_SELFTEST]))}),
    );
    let third = call(
        &mut session,
        false,
        &json!({"cmd": "execution_result", "status": "failure", "output": "boom"}),
    );
    assert_eq!(third["status"], "need_inference");
    assert!(third["request"]["messages"].to_string().contains("boom"));
}

#[test]
fn protocol_misuse_is_rejected_without_effect_and_without_echo() {
    let mut session = Session::new();
    for (document, code) in [
        (json!({"cmd": "cancel"}), "not_begun"),
        (json!({"v": 2}), "unsupported_version"),
        (
            json!({"v": 1, "work_id": "w", "goal": "g", "model": "m"}),
            "invalid_document",
        ),
        (
            json!({"v": 1, "work_id": "w", "goal": "g", "model": "m", "request_seed": "zz", "options_policy": "refuse"}),
            "invalid_document",
        ),
        (
            json!({"v": 1, "work_id": "w", "goal": "g", "model": "m", "request_seed": "0000000000000000", "options_policy": "sometimes"}),
            "invalid_document",
        ),
        (
            json!({"v": 1, "work_id": "w", "goal": "g", "model": "m", "request_seed": "0000000000000000", "options_policy": "refuse", "capabilities": [{"id": "Bad Id", "name": "x"}]}),
            "invalid_document",
        ),
    ] {
        let begin = document.get("v").is_some();
        let response = call(&mut session, begin, &document);
        assert_eq!(response["status"], "error", "{document}");
        assert_eq!(response["error"]["code"], code, "{document}");
        assert!(
            !response.to_string().contains("rm -rf"),
            "errors never echo input"
        );
    }
    // A good begin works after rejected ones; a second begin is refused; stray results are refused.
    let first = call(&mut session, true, &begin_document());
    assert_eq!(first["status"], "need_inference");
    assert_eq!(
        call(&mut session, true, &begin_document())["error"]["code"],
        "already_begun"
    );
    assert_eq!(
        call(
            &mut session,
            false,
            &json!({"cmd": "execution_result", "status": "success"})
        )["error"]["code"],
        "unexpected_command"
    );
    assert_eq!(
        call(&mut session, false, &json!({"cmd": "fly"}))["error"]["code"],
        "unknown_command"
    );
    let garbage = session.command(b"{not json");
    assert!(
        String::from_utf8(garbage)
            .unwrap()
            .contains("invalid_command")
    );
}
