#![allow(dead_code)]
//! A deterministic HTTP CONNECT proxy on loopback, std only. It records every request it is
//! asked to make (the `CONNECT` line and any `Proxy-Authorization`), can require Basic
//! credentials, and tunnels bytes to the target unchanged, so TLS inside the tunnel is end to
//! end exactly as through a real proxy.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// What the proxy saw for one connection.
#[derive(Clone, Debug)]
pub struct Seen {
    /// The request line, for example `CONNECT 127.0.0.1:4000 HTTP/1.1`.
    pub request_line: String,
    /// The `Proxy-Authorization` header value, if any.
    pub authorization: Option<String>,
}

pub struct Proxy {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Proxy {
    /// A proxy that tunnels every `CONNECT`. With `credentials`, a request without exactly
    /// `Basic base64(user:password)` is answered `407` and nothing is tunnelled.
    pub fn start(credentials: Option<(&str, &str)>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let expected =
            credentials.map(|(user, pass)| format!("Basic {}", base64(&format!("{user}:{pass}"))));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen_in, stop_in) = (seen.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stop_in.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (seen, expected) = (seen_in.clone(), expected.clone());
                        std::thread::spawn(move || handle(stream, &seen, expected.as_deref()));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            seen,
            stop,
            thread: Some(thread),
        }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle(mut client: TcpStream, seen: &Mutex<Vec<Seen>>, expected: Option<&str>) {
    let _ = client.set_nonblocking(false);
    let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match client.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let authorization = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("proxy-authorization")
            .then(|| value.trim().to_owned())
    });
    seen.lock().unwrap().push(Seen {
        request_line: request_line.clone(),
        authorization: authorization.clone(),
    });
    if let Some(expected) = expected
        && authorization.as_deref() != Some(expected)
    {
        let _ = client.write_all(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nproxy-authenticate: Basic realm=\"test\"\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
        let _ = client.shutdown(Shutdown::Both);
        return;
    }
    let Some(target) = request_line
        .strip_prefix("CONNECT ")
        .and_then(|rest| rest.split(' ').next())
    else {
        let _ = client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n");
        return;
    };
    let Ok(upstream) = TcpStream::connect(target) else {
        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n");
        return;
    };
    let _ = client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
    let _ = client.set_read_timeout(None);
    let (mut client_read, mut upstream_write) =
        (client.try_clone().unwrap(), upstream.try_clone().unwrap());
    let (mut upstream_read, mut client_write) = (upstream, client);
    let forward = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Both);
    let _ = forward.join();
}

/// Standard base64, for the expected `Proxy-Authorization` value.
pub fn base64(text: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
