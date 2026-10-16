//! The FX executor, the only executor, against a loopback mock that answers in the format FX asks for
//! (an event stream). Mocked transport only: nothing here talks to a live provider or uses TLS.
//!
//! These tests pin what is put on the wire, how failures are classified (one request, no retry, no key
//! in an error), what the executor refuses by name before any request, and how the executor is chosen.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chip_cli::provider_selection::{ExecutorKind, build_model};
use chip_inference::{FxError, Message, MessageRole, ModelRequest, Secret};
use chip_inference::{PROVIDER_ANTHROPIC, PROVIDER_OPENAI_COMPATIBLE, ProviderConfig};
use serde_json::{Value, json};

const KEY: &str = "sk-parity-0123456789abcdef";

// ---- a mock server that answers by request shape ---------------------------------------------

#[derive(Clone)]
enum Answer {
    /// Success: the text and (input, output) token counts.
    Text(&'static str, u32, u32),
    /// An HTTP error status with a JSON error body.
    Status(u16),
}

struct Server {
    base: String,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, c, st) = (seen.clone(), connections.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        c.fetch_add(1, Ordering::SeqCst);
                        serve(stream, &answer, &s);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            base,
            seen,
            connections,
            stop,
            thread: Some(thread),
        }
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// The (head, JSON body) of every request.
    fn requests(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, Value)> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let (mut data, mut buf) = (Vec::new(), [0u8; 2048]);
    let head_end = loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0usize);
    while data.len() < head_end + length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body = serde_json::from_slice(&data[head_end..]).unwrap_or(Value::Null);
    Some((head, body))
}

fn serve(mut stream: TcpStream, answer: &Answer, seen: &Arc<Mutex<Vec<(String, Value)>>>) {
    let Some((head, body)) = read_request(&mut stream) else {
        return;
    };
    let anthropic = head.starts_with("POST /v1/messages ");
    let streaming = body.get("stream") == Some(&Value::Bool(true));
    seen.lock().unwrap().push((head, body));
    let (status, content_type, payload): (u16, &str, String) = match answer {
        Answer::Status(code) => (
            *code,
            "application/json",
            if anthropic {
                r#"{"type":"error","error":{"type":"x","message":"mock failure"}}"#.to_owned()
            } else {
                r#"{"error":{"message":"mock failure"}}"#.to_owned()
            },
        ),
        Answer::Text(text, input, output) => match (anthropic, streaming) {
            (false, false) => (200, "application/json", json!({"id":"r1","choices":[{"message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":input,"completion_tokens":output}}).to_string()),
            (true, false) => (200, "application/json", json!({"id":"r1","content":[{"type":"text","text":text}],"usage":{"input_tokens":input,"output_tokens":output}}).to_string()),
            (false, true) => (200, "text/event-stream", format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":input,"completion_tokens":output}}),
            )),
            (true, true) => (200, "text/event-stream", [
                ("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":input}}})),
                ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})),
                ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":output}})),
                ("message_stop", json!({"type":"message_stop"})),
            ].iter().map(|(e, d)| format!("event: {e}\ndata: {d}\n\n")).collect()),
        },
    };
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(payload.as_bytes());
    let _ = stream.shutdown(Shutdown::Both);
}

// ---- fixtures ---------------------------------------------------------------------------------

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'static {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect();
    move |name| map.get(name).cloned()
}

fn config(provider: &str, base: &str) -> ProviderConfig {
    let path = if provider == PROVIDER_ANTHROPIC {
        "/v1/messages"
    } else {
        "/v1/chat/completions"
    };
    let mut config = ProviderConfig::new(provider, "mock-model", format!("{base}{path}"))
        .with_api_key(Secret::new(KEY))
        .with_timeout(Duration::from_secs(10));
    config.temperature = Some(0.0);
    config.max_tokens = Some(300);
    config
}

fn request() -> ModelRequest {
    let mut request = ModelRequest::new(
        "mock-model",
        vec![
            Message::new(MessageRole::System, "be brief"),
            Message::new(MessageRole::System, "and exact"),
            Message::new(MessageRole::User, "say hi"),
        ],
    );
    request.max_tokens = None;
    request.temperature = None;
    request
}

async fn call(
    config: ProviderConfig,
    executor: ExecutorKind,
) -> Result<chip_inference::ModelResponse, FxError> {
    let model = build_model(config, executor, &env_of(&[]))?;
    model.complete(request()).await
}

/// The coarse class of a failure, comparable across the two executors.
fn class(error: &FxError) -> &'static str {
    let text = error.to_string();
    let status = match error {
        FxError::Inference(f) => f.status,
        _ => (400..600).find(|code| text.contains(&format!("returned {code}"))),
    };
    match (error, status) {
        (FxError::Authentication(_), _) | (_, Some(401 | 403)) => "authentication",
        (_, Some(429)) => "rate limited",
        (_, Some(500..=599)) => "server error",
        _ => "other",
    }
}

const PROVIDERS: [&str; 2] = [PROVIDER_OPENAI_COMPATIBLE, PROVIDER_ANTHROPIC];

// ---- success ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_executor_returns_the_text_and_usage_with_one_request() {
    for provider in PROVIDERS {
        let server = Server::start(Answer::Text("hello there", 11, 4));
        let response = call(config(provider, &server.base), ExecutorKind::Fx)
            .await
            .unwrap();
        assert_eq!(server.connections(), 1, "{provider}: one request");
        assert_eq!(
            (
                response.output,
                response.usage.prompt_tokens,
                response.usage.completion_tokens
            ),
            ("hello there".to_owned(), 11, 4),
            "{provider}"
        );
    }
}

// ---- what is put on the wire ---------------------------------------------------------------

async fn wire(provider: &str) -> (String, Value) {
    let server = Server::start(Answer::Text("ok", 1, 1));
    call(config(provider, &server.base), ExecutorKind::Fx)
        .await
        .unwrap();
    server.requests().remove(0)
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_openai_request_carries_the_model_messages_cap_and_temperature() {
    let (fx_head, fx) = wire(PROVIDER_OPENAI_COMPATIBLE).await;
    assert_eq!(fx["model"], "mock-model");
    // The consecutive system messages are one message.
    assert_eq!(
        fx["messages"][0],
        json!({"role": "system", "content": "be brief\n\nand exact"})
    );
    assert_eq!(
        fx["messages"][1],
        json!({"role": "user", "content": "say hi"})
    );
    assert_eq!(fx["max_tokens"], 300);
    assert_eq!(
        fx["temperature"], 0.0,
        "a configured zero temperature is sent, not dropped"
    );
    assert_eq!(
        header(&fx_head, "authorization").as_deref(),
        Some(&*format!("Bearer {KEY}"))
    );
    // FX asks for a stream.
    assert_eq!(fx["stream"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_anthropic_request_carries_the_model_system_cap_and_temperature() {
    let (fx_head, fx) = wire(PROVIDER_ANTHROPIC).await;
    assert_eq!(fx["model"], "mock-model");
    assert_eq!(fx["system"], "be brief\n\nand exact");
    assert_eq!(fx["max_tokens"], 300);
    assert_eq!(fx["temperature"], 0.0);
    assert!(header(&fx_head, "anthropic-version").is_some());
    assert_eq!(header(&fx_head, "x-api-key").as_deref(), Some(KEY));
}

#[tokio::test(flavor = "multi_thread")]
async fn json_object_output_is_requested_on_openai_compatible_endpoints() {
    let server = Server::start(Answer::Text("{}", 1, 1));
    let config = config(PROVIDER_OPENAI_COMPATIBLE, &server.base).with_json_object_output();
    call(config, ExecutorKind::Fx).await.unwrap();
    let body = &server.requests()[0].1;
    // FX expresses structured output as a schema, here "an object".
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(
        body["response_format"]["json_schema"]["schema"],
        json!({"type": "object"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_ignores_the_json_object_flag() {
    let server = Server::start(Answer::Text("ok", 1, 1));
    let config = config(PROVIDER_ANTHROPIC, &server.base).with_json_object_output();
    call(config, ExecutorKind::Fx).await.unwrap();
    assert_eq!(server.requests()[0].1.get("response_format"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_anthropic_workspace_header_and_fallback_cap_are_sent() {
    let server = Server::start(Answer::Text("ok", 1, 1));
    let mut config = config(PROVIDER_ANTHROPIC, &server.base).with_workspace_id("wrkspc_1");
    config.max_tokens = None;
    call(config, ExecutorKind::Fx).await.unwrap();
    let (head, body) = &server.requests()[0];
    assert_eq!(
        header(head, "anthropic-workspace-id").as_deref(),
        Some("wrkspc_1")
    );
    assert_eq!(body["max_tokens"], 1024, "the fallback cap");
}

// ---- failures ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn http_errors_fail_in_their_class_with_one_request_and_no_key_in_the_error() {
    for provider in PROVIDERS {
        for (status, expected) in [
            (401u16, "authentication"),
            (403, "authentication"),
            (429, "rate limited"),
            (500, "server error"),
            (503, "server error"),
        ] {
            let server = Server::start(Answer::Status(status));
            let error = call(config(provider, &server.base), ExecutorKind::Fx)
                .await
                .unwrap_err();
            assert_eq!(server.connections(), 1, "{provider} {status}: no retry");
            let shown = format!("{error} {error:?}");
            assert!(
                !shown.contains(KEY) && !shown.contains("0123456789abcdef"),
                "{shown}"
            );
            assert_eq!(class(&error), expected, "{provider} {status}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_carries_its_delivery_certainty() {
    let server = Server::start(Answer::Status(500));
    let error = call(
        config(PROVIDER_OPENAI_COMPATIBLE, &server.base),
        ExecutorKind::Fx,
    )
    .await
    .unwrap_err();
    let FxError::Inference(failure) = error else {
        panic!("expected an inference failure")
    };
    assert_eq!(failure.delivery, chip_inference::Delivery::PossiblySent);
    assert_eq!(failure.status, Some(500));
}

// ---- selection ---------------------------------------------------------------------------------

#[test]
fn fx_is_the_only_executor_and_anything_else_is_refused_before_any_request() {
    let none = env_of(&[]);
    assert_eq!(ExecutorKind::choose(None, &none).unwrap(), ExecutorKind::Fx);
    assert_eq!(
        ExecutorKind::choose(Some("fx"), &none).unwrap(),
        ExecutorKind::Fx
    );
    assert_eq!(
        ExecutorKind::choose(None, &env_of(&[("CHIP_EXECUTOR", "fx")])).unwrap(),
        ExecutorKind::Fx
    );
    assert_eq!(
        ExecutorKind::choose(None, &env_of(&[("CHIP_EXECUTOR", "  ")])).unwrap(),
        ExecutorKind::Fx,
        "a blank value is no value"
    );
    // The removed `legacy` value, an unknown value, and a flag that disagrees with the environment
    // are all refused, in the flag and in the environment, and the message says FX is the only one.
    for refused in ["legacy", "LEGACY", "both", "FX", "fx,legacy", "ollama", "0"] {
        for result in [
            ExecutorKind::choose(Some(refused), &none),
            ExecutorKind::choose(None, &env_of(&[("CHIP_EXECUTOR", refused)])),
            ExecutorKind::choose(Some(refused), &env_of(&[("CHIP_EXECUTOR", "fx")])),
        ] {
            let message = result.unwrap_err().to_string();
            assert!(
                message.contains("FX is the only supported executor"),
                "{refused}: {message}"
            );
        }
    }
    // A valid flag does not rescue a stale environment value: both are validated.
    assert!(ExecutorKind::choose(Some("fx"), &env_of(&[("CHIP_EXECUTOR", "legacy")])).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn what_the_fx_executor_cannot_carry_is_refused_by_name_before_any_request() {
    let server = Server::start(Answer::Text("never", 1, 1));
    let refuse = |config: ProviderConfig| {
        build_model(config, ExecutorKind::Fx, &env_of(&[]))
            .err()
            .map(|e| e.to_string())
    };

    let ollama = ProviderConfig::new("ollama", "m", format!("{}/api/chat", server.base));
    assert!(refuse(ollama).unwrap().contains("ollama"));

    let thinking = config(PROVIDER_ANTHROPIC, &server.base).with_enable_thinking(true);
    let key = env_of(&[("ANTHROPIC_API_KEY", "sk-test")]);
    let refused = build_model(thinking, ExecutorKind::Fx, &key)
        .err()
        .map(|e| e.to_string());
    assert!(refused.unwrap().contains("CHIP_ENABLE_THINKING"));

    let mut odd = config(PROVIDER_OPENAI_COMPATIBLE, &server.base);
    odd.endpoint = format!("{}/custom/path", server.base);
    assert!(refuse(odd).unwrap().contains("/chat/completions"));
    assert_eq!(server.connections(), 0, "nothing was sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_key_fails_before_any_request_and_names_the_variable() {
    let server = Server::start(Answer::Text("never", 1, 1));
    let mut anthropic = config(PROVIDER_ANTHROPIC, &server.base);
    anthropic.api_key = None;
    // The mock is on loopback, but Anthropic always needs a key.
    let error = build_model(anthropic, ExecutorKind::Fx, &env_of(&[]))
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("ANTHROPIC_API_KEY") && error.contains("no request was made"),
        "{error}"
    );

    // The provider's own variable satisfies it, and CHIP_API_KEY is not needed.
    let mut anthropic = config(PROVIDER_ANTHROPIC, &server.base);
    anthropic.api_key = None;
    let model = build_model(
        anthropic,
        ExecutorKind::Fx,
        &env_of(&[("ANTHROPIC_API_KEY", KEY)]),
    )
    .unwrap();
    model.complete(request()).await.unwrap();
    assert_eq!(server.connections(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn lm_studio_on_loopback_needs_no_key_in_the_fx_executor() {
    let server = Server::start(Answer::Text("local", 2, 1));
    let mut lm = config(PROVIDER_OPENAI_COMPATIBLE, &server.base);
    lm.api_key = None;
    let model = build_model(lm, ExecutorKind::Fx, &env_of(&[])).unwrap();
    assert_eq!(model.complete(request()).await.unwrap().output, "local");
    assert_eq!(header(&server.requests()[0].0, "authorization"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thinking_switch_is_a_top_level_template_field_in_both_states_and_unset() {
    for setting in [None, Some(false), Some(true)] {
        let server = Server::start(Answer::Text("ok", 1, 1));
        let mut config = config(PROVIDER_OPENAI_COMPATIBLE, &server.base);
        if let Some(enable) = setting {
            config = config.with_enable_thinking(enable);
        }
        call(config, ExecutorKind::Fx).await.unwrap();
        let body = server.requests()[0].1.clone();
        let expected = setting.map(|enable| json!({ "enable_thinking": enable }));
        assert_eq!(
            body.get("chat_template_kwargs"),
            expected.as_ref(),
            "{setting:?}"
        );
        assert_eq!(body.get("extra_body"), None);
        // A request field, never prompt text.
        assert!(!body["messages"].to_string().contains("enable_thinking"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thinking_switch_on_anthropic_is_refused_by_the_fx_executor_before_any_request() {
    let server = Server::start(Answer::Text("never", 1, 1));
    let config = config(PROVIDER_ANTHROPIC, &server.base).with_enable_thinking(false);
    // The executor names the switch instead of dropping it.
    let before = server.connections();
    let error = call(config, ExecutorKind::Fx)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("CHIP_ENABLE_THINKING"), "{error}");
    assert_eq!(server.connections(), before, "nothing was sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn lan_http_is_refused_by_default_and_the_opt_in_is_strict_about_its_value_and_keys() {
    let lan = ProviderConfig::new(
        PROVIDER_OPENAI_COMPATIBLE,
        "m",
        "http://192.168.1.20:1234/v1/chat/completions",
    );
    let build = |env: &[(&str, &str)]| {
        build_model(lan.clone(), ExecutorKind::Fx, &env_of(env))
            .err()
            .map(|e| e.to_string())
    };
    assert!(build(&[]).unwrap().contains("CHIP_ALLOW_INSECURE_LAN_HTTP"));
    assert!(
        build(&[("CHIP_ALLOW_INSECURE_LAN_HTTP", "0")])
            .unwrap()
            .contains("plain http")
    );
    assert!(
        build(&[("CHIP_ALLOW_INSECURE_LAN_HTTP", "yes")])
            .unwrap()
            .contains("`1` or `0`")
    );
    // Keyless with the opt-in builds.
    assert!(build(&[("CHIP_ALLOW_INSECURE_LAN_HTTP", "1")]).is_none());
    // Any key, from the provider variable or from CHIP_API_KEY, makes the opt-in inapplicable.
    let with_key = build(&[
        ("CHIP_ALLOW_INSECURE_LAN_HTTP", "1"),
        ("OPENAI_API_KEY", "sk-x"),
    ]);
    assert!(with_key.unwrap().contains("refusing to send a credential"));
    let chip_key = ProviderConfig::new(
        PROVIDER_OPENAI_COMPATIBLE,
        "m",
        "http://192.168.1.20:1234/v1/chat/completions",
    )
    .with_api_key(chip_inference::Secret::new("sk-chip".to_owned()));
    let text = build_model(
        chip_key,
        ExecutorKind::Fx,
        &env_of(&[("CHIP_ALLOW_INSECURE_LAN_HTTP", "1")]),
    )
    .err()
    .map(|e| e.to_string())
    .unwrap();
    assert!(
        text.contains("refusing to send a credential") && !text.contains("sk-chip"),
        "{text}"
    );
    // A public address passes resolution but the provider refuses to be built.
    let public = ProviderConfig::new(
        PROVIDER_OPENAI_COMPATIBLE,
        "m",
        "http://8.8.8.8:1234/v1/chat/completions",
    );
    let refused = build_model(
        public,
        ExecutorKind::Fx,
        &env_of(&[("CHIP_ALLOW_INSECURE_LAN_HTTP", "1")]),
    )
    .err()
    .map(|e| e.to_string());
    assert!(refused.unwrap().contains("loopback"));
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_flag_and_no_environment_the_request_goes_through_the_fx_executor() {
    let server = Server::start(Answer::Text("ok", 1, 1));
    let executor = ExecutorKind::choose(None, &env_of(&[])).unwrap();
    call(config(PROVIDER_OPENAI_COMPATIBLE, &server.base), executor)
        .await
        .unwrap();
    // The fx wire format streams.
    assert_eq!(server.requests()[0].1["stream"], json!(true));
}
