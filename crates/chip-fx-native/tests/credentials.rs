//! Credential resolution for the native providers: environment variables, no real secrets.
//!
//! Chip acquires the credential (`CredentialSource` to resolved key to `NativeSettings`); FX's
//! native provider only consumes it. Every test supplies its own fake environment.

mod mock;

use std::collections::HashMap;

use chip_fx::OptionsPolicy;
use chip_fx_native::{
    EnvCredentials, NativeModelProvider, NativeSelection, NativeSettings, ProviderKind,
};
use chip_inference::{FxError, Message, MessageRole, ModelProvider, ModelRequest};
use mock::{Reply, Server, anthropic_reply, reply};

const ANTHROPIC_KEY: &str = "sk-ant-credtest-0123456789abcdef";
const OPENAI_KEY: &str = "sk-credtest-openai-0123456789abcdef";
const ROUTER_KEY: &str = "sk-or-credtest-0123456789abcdef";

fn env(pairs: &[(&str, &str)]) -> EnvCredentials<impl Fn(&str) -> Option<String>> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    EnvCredentials::new(move |name| map.get(name).cloned())
}

fn resolve(selection: &NativeSelection, pairs: &[(&str, &str)]) -> Result<NativeSettings, FxError> {
    NativeSettings::resolve(
        selection,
        &env(pairs),
        OptionsPolicy::DropUnsupported,
        *b"chip-crd",
    )
}

fn selection(kind: ProviderKind, endpoint: &str) -> NativeSelection {
    NativeSelection {
        endpoint: Some(endpoint.to_owned()),
        ..NativeSelection::new(kind)
    }
}

fn request() -> ModelRequest {
    ModelRequest::new("mock-1", vec![Message::new(MessageRole::User, "say hi")])
}

// ---- resolution -----------------------------------------------------------------------------

#[test]
fn each_provider_reads_its_own_default_variable_and_endpoint() {
    let anthropic = resolve(
        &NativeSelection::new(ProviderKind::Anthropic),
        &[
            ("ANTHROPIC_API_KEY", ANTHROPIC_KEY),
            ("OPENAI_API_KEY", OPENAI_KEY),
        ],
    )
    .unwrap();
    assert_eq!(anthropic.api_key.as_deref(), Some(ANTHROPIC_KEY));
    assert_eq!(anthropic.base_url, "https://api.anthropic.com/v1");

    let openai = resolve(
        &NativeSelection::new(ProviderKind::OpenAiCompatible),
        &[
            ("ANTHROPIC_API_KEY", ANTHROPIC_KEY),
            ("OPENAI_API_KEY", OPENAI_KEY),
        ],
    )
    .unwrap();
    assert_eq!(openai.api_key.as_deref(), Some(OPENAI_KEY));
    assert_eq!(openai.base_url, "https://api.openai.com/v1");
}

#[test]
fn an_alternative_variable_is_used_alone_and_never_falls_back_to_the_default() {
    let mut openrouter = selection(
        ProviderKind::OpenAiCompatible,
        "https://openrouter.ai/api/v1",
    );
    openrouter.key_env = Some("OPENROUTER_API_KEY".into());
    let resolved = resolve(
        &openrouter,
        &[
            ("OPENROUTER_API_KEY", ROUTER_KEY),
            ("OPENAI_API_KEY", OPENAI_KEY),
        ],
    )
    .unwrap();
    assert_eq!(resolved.api_key.as_deref(), Some(ROUTER_KEY));

    // OPENROUTER_API_KEY unset: OPENAI_API_KEY must not be sent to OpenRouter in its place.
    let error = resolve(&openrouter, &[("OPENAI_API_KEY", OPENAI_KEY)]).unwrap_err();
    let FxError::Configuration(message) = &error else {
        panic!("{error:?}")
    };
    assert!(message.contains("OPENROUTER_API_KEY"), "{message}");
    assert!(!format!("{error:?}").contains(OPENAI_KEY));
}

#[test]
fn a_missing_key_is_an_actionable_error_before_any_network_request() {
    for (kind, variable) in [
        (ProviderKind::Anthropic, "ANTHROPIC_API_KEY"),
        (ProviderKind::OpenAiCompatible, "OPENAI_API_KEY"),
    ] {
        let error = resolve(&NativeSelection::new(kind), &[]).unwrap_err();
        let FxError::Configuration(message) = error else {
            panic!("expected a configuration error")
        };
        assert!(message.contains(variable), "{message}");
        assert!(message.contains("no request was made"), "{message}");
    }
}

#[test]
fn blank_values_count_as_unset_and_values_are_trimmed() {
    let anthropic = NativeSelection::new(ProviderKind::Anthropic);
    assert!(resolve(&anthropic, &[("ANTHROPIC_API_KEY", "   ")]).is_err());
    assert!(resolve(&anthropic, &[("ANTHROPIC_API_KEY", "")]).is_err());
    let padded = format!("  {ANTHROPIC_KEY}\n");
    let resolved = resolve(&anthropic, &[("ANTHROPIC_API_KEY", &padded)]).unwrap();
    assert_eq!(resolved.api_key.as_deref(), Some(ANTHROPIC_KEY));
}

#[test]
fn a_variable_name_that_is_not_one_is_refused() {
    let mut s = NativeSelection::new(ProviderKind::OpenAiCompatible);
    for bad in ["", "open ai", "1KEY", "KEY=1", "lower"] {
        s.key_env = Some(bad.into());
        assert!(
            matches!(resolve(&s, &[]), Err(FxError::Configuration(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn a_local_endpoint_needs_no_key_and_uses_one_if_it_is_set() {
    let lm_studio = selection(ProviderKind::OpenAiCompatible, "http://localhost:1235/v1");
    let keyless = resolve(&lm_studio, &[]).unwrap();
    assert_eq!(keyless.api_key, None);
    let keyed = resolve(&lm_studio, &[("OPENAI_API_KEY", OPENAI_KEY)]).unwrap();
    assert_eq!(keyed.api_key.as_deref(), Some(OPENAI_KEY));
    for local in ["http://127.0.0.1:1234/v1", "http://[::1]:1234/v1"] {
        assert!(
            resolve(&selection(ProviderKind::OpenAiCompatible, local), &[]).is_ok(),
            "{local}"
        );
    }
    // Not local: a key is required, whatever the scheme.
    for remote in [
        "https://lm.example.test/v1",
        "http://lm.example.test/v1",
        "http://localhost.example.test/v1",
    ] {
        assert!(
            resolve(&selection(ProviderKind::OpenAiCompatible, remote), &[]).is_err(),
            "{remote}"
        );
    }
}

#[test]
fn anthropic_always_needs_a_key_even_on_a_local_endpoint() {
    let local = selection(ProviderKind::Anthropic, "http://localhost:8080/v1");
    assert!(resolve(&local, &[]).is_err());
}

// ---- redaction ------------------------------------------------------------------------------

#[test]
fn settings_and_errors_never_print_the_key() {
    let mut s = NativeSelection::new(ProviderKind::Anthropic);
    s.headers = vec![("anthropic-workspace-id".into(), "wrkspc_1".into())];
    let settings = resolve(&s, &[("ANTHROPIC_API_KEY", ANTHROPIC_KEY)]).unwrap();
    let shown = format!("{settings:?} {s:?}");
    assert!(
        !shown.contains(ANTHROPIC_KEY) && !shown.contains("0123456789abcdef"),
        "{shown}"
    );
    assert!(shown.contains("[redacted]"));
}

// ---- end to end with a mock server ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_resolved_from_the_environment_calls_the_messages_api_with_x_api_key() {
    let server = Server::start(vec![anthropic_reply("hello from claude")]);
    let s = selection(ProviderKind::Anthropic, &server.base_url);
    let settings = resolve(&s, &[("ANTHROPIC_API_KEY", ANTHROPIC_KEY)]).unwrap();
    let provider = NativeModelProvider::new(settings).unwrap();
    let response = provider.complete(request()).await.unwrap();
    assert_eq!(response.output, "hello from claude");
    assert_eq!(response.usage.prompt_tokens, 9);
    let (head, body) = &server.recorded()[0];
    let head = head.to_ascii_lowercase();
    assert!(head.starts_with("post /v1/messages "), "{head}");
    assert!(head.contains(&format!(
        "x-api-key: {}",
        ANTHROPIC_KEY.to_ascii_lowercase()
    )));
    assert!(!body.contains(ANTHROPIC_KEY));
    assert_eq!(server.connections(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn lm_studio_without_a_key_sends_no_authorization_header() {
    let server = Server::start(vec![reply("local answer")]);
    let s = selection(ProviderKind::OpenAiCompatible, &server.base_url);
    let settings = resolve(&s, &[]).unwrap();
    let provider = NativeModelProvider::new(settings).unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap().output,
        "local answer"
    );
    let head = server.recorded()[0].0.to_ascii_lowercase();
    assert!(!head.contains("authorization"), "{head}");
}

#[tokio::test(flavor = "multi_thread")]
async fn openrouter_attribution_headers_and_its_own_key_are_sent() {
    let server = Server::start(vec![reply("routed answer")]);
    // A loopback stand-in for https://openrouter.ai/api/v1 with a key from its own variable.
    let mut s = selection(ProviderKind::OpenAiCompatible, &server.base_url);
    s.key_env = Some("OPENROUTER_API_KEY".into());
    s.headers = vec![
        ("HTTP-Referer".into(), "https://example.test".into()),
        ("X-Title".into(), "Chip".into()),
    ];
    let settings = resolve(
        &s,
        &[
            ("OPENROUTER_API_KEY", ROUTER_KEY),
            ("OPENAI_API_KEY", OPENAI_KEY),
        ],
    )
    .unwrap();
    let provider = NativeModelProvider::new(settings).unwrap();
    provider.complete(request()).await.unwrap();
    let head = server.recorded()[0].0.to_ascii_lowercase();
    assert!(head.contains(&format!(
        "authorization: bearer {}",
        ROUTER_KEY.to_ascii_lowercase()
    )));
    assert!(
        !head.contains(&OPENAI_KEY.to_ascii_lowercase()),
        "the default variable's key was not used"
    );
    assert!(
        head.contains("http-referer: https://example.test"),
        "{head}"
    );
    assert!(head.contains("x-title: chip"), "{head}");
}

#[test]
fn headers_the_adapter_owns_are_rejected_when_the_provider_is_built() {
    for owned in [
        "Authorization",
        "x-api-key",
        "Content-Type",
        "anthropic-version",
        "Host",
    ] {
        let mut s = selection(ProviderKind::OpenAiCompatible, "http://localhost:1235/v1");
        s.headers = vec![(owned.into(), "x".into())];
        let settings = resolve(&s, &[]).unwrap();
        assert!(
            matches!(
                NativeModelProvider::new(settings),
                Err(FxError::Configuration(_))
            ),
            "{owned}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_error_that_echoes_the_key_never_reaches_the_chip_error() {
    let body =
        format!(r#"{{"type":"error","error":{{"message":"invalid x-api-key {ANTHROPIC_KEY}"}}}}"#);
    let server = Server::start(vec![Reply::Status(401, vec![], body)]);
    let s = selection(ProviderKind::Anthropic, &server.base_url);
    let settings = resolve(&s, &[("ANTHROPIC_API_KEY", ANTHROPIC_KEY)]).unwrap();
    let provider = NativeModelProvider::new(settings).unwrap();
    let error = provider.complete(request()).await.unwrap_err();
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains(ANTHROPIC_KEY) && !shown.contains("0123456789abcdef"),
        "{shown}"
    );
    assert_eq!(server.connections(), 1);
}
