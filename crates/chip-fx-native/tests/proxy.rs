//! chip-rs#35: model calls through an explicit HTTP `CONNECT` proxy, Chip to FX to the native
//! provider, against a loopback proxy and mock server. Mocked transport only.

mod mock;
mod proxy_server;

use chip_fx::OptionsPolicy;
use chip_fx_native::{NativeModelProvider, NativeSettings, ProviderKind, ProxySettings};
use chip_inference::{FxError, Message, MessageRole, ModelProvider, ModelRequest};
use mock::{Server, reply};
use proxy_server::{Proxy, base64};

const PROXY_PASSWORD: &str = "proxy-pw-0123456789";

fn settings(server: &Server, proxy: ProxySettings) -> NativeSettings {
    NativeSettings {
        kind: ProviderKind::OpenAiCompatible,
        base_url: server.base_url.clone(),
        // A credential is never sent over plain http through a proxy, so the loopback mock is keyless.
        api_key: None,
        headers: vec![],
        options: OptionsPolicy::DropUnsupported,
        budget: None,
        default_max_tokens: None,
        json_object_output: false,
        template_thinking: None,
        allow_insecure_lan_http: false,
        root_certificates_pem: None,
        proxy: Some(proxy),
        id_seed: *b"chip-prx",
    }
}

fn request() -> ModelRequest {
    ModelRequest::new("mock-1", vec![Message::new(MessageRole::User, "say hi")])
}

fn target(server: &Server) -> String {
    let rest = server.base_url.trim_start_matches("http://");
    rest.split('/').next().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_call_goes_through_the_configured_proxy() {
    let server = Server::start(vec![reply("through the proxy")]);
    let proxy = Proxy::start(Some(("chip", PROXY_PASSWORD)));
    let provider = NativeModelProvider::new(settings(
        &server,
        ProxySettings {
            url: proxy.url.clone(),
            credentials: Some(("chip".into(), PROXY_PASSWORD.into())),
            no_proxy: vec![],
        },
    ))
    .unwrap();
    let response = provider.complete(request()).await.unwrap();
    assert_eq!(response.output, "through the proxy");
    let seen = proxy.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(
        seen[0].request_line,
        format!("CONNECT {} HTTP/1.1", target(&server))
    );
    let expected = format!("Basic {}", base64(&format!("chip:{PROXY_PASSWORD}")));
    assert_eq!(seen[0].authorization.as_deref(), Some(expected.as_str()));
    assert_eq!(server.connections(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_no_proxy_host_is_reached_directly() {
    let server = Server::start(vec![reply("direct")]);
    let proxy = Proxy::start(None);
    let provider = NativeModelProvider::new(settings(
        &server,
        ProxySettings {
            url: proxy.url.clone(),
            credentials: None,
            no_proxy: vec!["127.0.0.1".into()],
        },
    ))
    .unwrap();
    assert_eq!(provider.complete(request()).await.unwrap().output, "direct");
    assert!(proxy.seen().is_empty());
    assert_eq!(server.connections(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_proxy_fails_the_call_with_its_delivery_and_no_direct_fallback() {
    let server = Server::start(vec![reply("never")]);
    let proxy = Proxy::start(Some(("chip", "the-right-one")));
    let provider = NativeModelProvider::new(settings(
        &server,
        ProxySettings {
            url: proxy.url.clone(),
            credentials: Some(("chip".into(), PROXY_PASSWORD.into())),
            no_proxy: vec![],
        },
    ))
    .unwrap();
    let error = provider.complete(request()).await.unwrap_err();
    let FxError::Inference(failure) = &error else {
        panic!("expected an FX failure, got {error:?}")
    };
    assert_eq!(failure.kind, "transport");
    let text = format!("{error} {error:?}");
    assert!(!text.contains(PROXY_PASSWORD), "{text}");
    assert_eq!(server.connections(), 0, "the endpoint was reached directly");
}

#[test]
fn the_proxy_password_is_never_printed_from_the_settings() {
    let server = Server::start(vec![]);
    let s = settings(
        &server,
        ProxySettings {
            url: "http://proxy.corp:3128".into(),
            credentials: Some(("chip".into(), PROXY_PASSWORD.into())),
            no_proxy: vec![],
        },
    );
    let printed = format!("{s:?}");
    assert!(printed.contains("proxy.corp") && printed.contains("chip"));
    assert!(!printed.contains(PROXY_PASSWORD), "{printed}");
}
