//! Minimal one-shot-per-connection mock HTTP server for offline tests.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Captured {
    pub request_line: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct MockServer {
    pub url: String,
    pub captured: Arc<Mutex<Vec<Captured>>>,
}

/// Starts a server answering every request with `status` and `body`
/// after `delay`.
pub async fn start(status: u16, body: &str, delay: Duration) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let captured = Arc::new(Mutex::new(Vec::new()));
    let store = captured.clone();
    let body = body.to_string();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let store = store.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, content_len) = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while buf.len() < head_end + content_len {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end - 4]).to_string();
                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or("").to_string();
                let headers = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();
                let req_body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                store.lock().await.push(Captured {
                    request_line,
                    headers,
                    body: req_body,
                });

                tokio::time::sleep(delay).await;
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    MockServer { url, captured }
}

pub const OK_BODY: &str = r#"{"id":"resp-1","choices":[{"message":{"role":"assistant","content":"Hello from the mock."}}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;
