//! A loopback HTTP server for the native-provider tests: scripted replies, one per connection,
//! and a record of every request. No external network and no real credentials.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub enum Reply {
    Sse(Vec<Vec<u8>>),
    Status(u16, Vec<(&'static str, String)>, String),
    /// Headers and the chunks, then the connection is held open.
    Stall(Vec<Vec<u8>>),
    /// Nothing is answered; the connection is held open.
    Silent,
}

pub struct Server {
    pub base_url: String,
    pub requests: Arc<Mutex<Vec<(String, String)>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, c, s) = (requests.clone(), connections.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut script = script.into_iter();
            let mut held: Vec<TcpStream> = Vec::new();
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        c.fetch_add(1, Ordering::SeqCst);
                        if let Some(reply) = script.next()
                            && let Some(kept) = serve(stream, reply, &r)
                        {
                            held.push(kept);
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
            for stream in held {
                let _ = stream.shutdown(Shutdown::Both);
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
            connections,
            stop,
            thread: Some(thread),
        }
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn recorded(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap().clone()
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

fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut data = Vec::new();
    let mut buf = [0u8; 2048];
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
                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    while data.len() < head_end + length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Some((
        head,
        String::from_utf8_lossy(&data[head_end..]).into_owned(),
    ))
}

fn serve(
    mut stream: TcpStream,
    reply: Reply,
    requests: &Arc<Mutex<Vec<(String, String)>>>,
) -> Option<TcpStream> {
    let request = read_request(&mut stream)?;
    requests.lock().unwrap().push(request);
    match reply {
        Reply::Sse(chunks) => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            );
            for chunk in chunks {
                let _ = stream.write_all(&chunk);
                let _ = stream.flush();
                std::thread::sleep(Duration::from_millis(3));
            }
            let _ = stream.shutdown(Shutdown::Both);
            None
        }
        Reply::Status(status, headers, body) => {
            let mut head = format!(
                "HTTP/1.1 {status} X\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
                body.len()
            );
            for (k, v) in headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.shutdown(Shutdown::Both);
            None
        }
        Reply::Stall(chunks) => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            );
            for chunk in chunks {
                let _ = stream.write_all(&chunk);
                let _ = stream.flush();
            }
            Some(stream)
        }
        Reply::Silent => Some(stream),
    }
}

pub fn delta(text: &str) -> Vec<u8> {
    format!(
        "data: {{\"model\":\"mock-1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\n",
        serde_json::to_string(text).unwrap()
    )
    .into_bytes()
}

pub fn finish() -> Vec<u8> {
    b"data: {\"model\":\"mock-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_vec()
}

pub fn usage(input: u32, output: u32) -> Vec<u8> {
    format!("data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{input},\"completion_tokens\":{output}}}}}\n\n").into_bytes()
}

pub fn done() -> Vec<u8> {
    b"data: [DONE]\n\n".to_vec()
}

pub fn reply(text: &str) -> Reply {
    Reply::Sse(vec![delta(text), finish(), usage(11, 4), done()])
}

// ---- Anthropic Messages API stream -----------------------------------------------------------

fn aevent(name: &str, json: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {json}\n\n").into_bytes()
}

/// A complete Anthropic reply: start, one text delta, stop reason with usage, stop.
pub fn anthropic_reply(text: &str) -> Reply {
    Reply::Sse(vec![
        aevent(
            "message_start",
            r#"{"type":"message_start","message":{"model":"claude-mock","usage":{"input_tokens":9}}}"#,
        ),
        aevent(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":{}}}}}"#,
                serde_json::to_string(text).unwrap()
            ),
        ),
        aevent(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
        ),
        aevent("message_stop", r#"{"type":"message_stop"}"#),
    ])
}
