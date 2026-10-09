//! How much does `chip serve` keep for work that has already finished?
//!
//! Independent of any persistence: this runs the real `chip` binary as a local application would
//! (`chip serve`, driven over TCP), submits many bounded work items one wave at a time, lets each
//! finish, and reads the server process's own memory from `/proc/<pid>/status`. The model is a
//! local mock HTTP server with one fixed reply and PAX is a shim that only identifies itself, so
//! this measures the *service's retention of finished work*, not any model or PAX behavior.
//!
//! Two scenarios, both bounded runs:
//!
//! * `escape`: the model asks for a write outside the project, which Chip refuses; the run ends
//!   blocked after one turn. The smallest possible result (what the serve tests use).
//! * `read`: the model keeps asking to read a project file of `--file-kib` KiB until the turn limit
//!   ends the run. A result with more turns and more events.
//!
//! `cargo bench -p chip-session-memory --bench service_retention -- [--works N] [--file-kib K]
//! [--limits 64,1000000] [--bin PATH] [--out PATH]`. Each scenario runs once per retention limit
//! (`chip serve --max-retained-work`); the largest limit stands in for the unbounded behavior the
//! service had before retention was bounded. Build the binary first: `cargo build --release -p chip-cli`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn status_kib(pid: u32, field: &str) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix(field).map(str::to_string))
        })
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

fn completion(content: &str) -> String {
    json!({
        "id": "resp-retention",
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 4},
    })
    .to_string()
}

/// A minimal HTTP server that answers every request with `body`.
fn mock_model(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let body = body.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                // Read the head, then the declared body, then answer.
                loop {
                    let n = match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    format!("http://{addr}")
}

fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let body = body.unwrap_or("");
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        body.to_string(),
    )
}

fn run_scenario(
    bin: &PathBuf,
    scenario: &str,
    works: usize,
    file_kib: usize,
    max_retained: usize,
) -> Value {
    let tmp =
        std::env::temp_dir().join(format!("chip-retention-{}-{scenario}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    let mut source = String::from("pub fn one() -> u8 { 1 }\n");
    while source.len() < file_kib * 1024 {
        source.push_str("// filler line to give the file a realistic size for a read\n");
    }
    std::fs::write(tmp.join("src/lib.rs"), &source).unwrap();
    let shim = tmp.join("pax");
    std::fs::write(&shim, "#!/bin/sh\necho 'pax 9.9.9'\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let decision = match scenario {
        "escape" => {
            r#"{"decision":"request_capability","capability":"project.write","inputs":{"path":"../chip-retention-escaped.txt","content":"x"}}"#
        }
        "read" => {
            r#"{"decision":"request_capability","capability":"project.read","inputs":{"path":"src/lib.rs"}}"#
        }
        other => panic!("unknown scenario {other}"),
    };
    let model = mock_model(completion(decision));
    let mut child = Command::new(bin)
        .args(["serve", "--host", "127.0.0.1", "--port", "0"])
        .args(["--max-retained-work", &max_retained.to_string()])
        .current_dir(&tmp)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", &model)
        .env("CHIP_API_KEY", "sk-retention-not-a-secret")
        .env("PAX_BIN", &shim)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("chip serve starts");
    let pid = child.id();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let addr = line
        .trim()
        .strip_prefix("Chip Runtime Service listening on http://")
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
        .to_string();

    let sample = |label: &str, finished: usize| {
        let (_, m) = http(&addr, "GET", "/v1/metrics", None);
        let m: Value = serde_json::from_str(&m).unwrap_or(Value::Null);
        json!({"label": label, "finished": finished,
               "retained_work": m["retained_work"], "evicted_work": m["evicted_work"],
               "submitted_work": m["submitted_work"],
               "rss_kib": status_kib(pid, "VmRSS:"), "rss_anon_kib": status_kib(pid, "RssAnon:"),
               "hwm_kib": status_kib(pid, "VmHWM:")})
    };

    // Warm up with a few items so one-time allocations are not charged to retention.
    let mut ids: Vec<String> = Vec::new();
    let run_wave = |count: usize, ids: &mut Vec<String>| {
        let mut wave = Vec::new();
        for _ in 0..count {
            loop {
                let (status, body) = http(
                    &addr,
                    "POST",
                    "/v1/work",
                    Some(r#"{"goal":"Add a function that sorts the payload."}"#),
                );
                if status == 202 {
                    let v: Value = serde_json::from_str(&body).unwrap();
                    wave.push(v["work_id"].as_str().unwrap().to_string());
                    break;
                }
                assert_eq!(status, 429, "{body}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        for id in &wave {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let (status, body) = http(&addr, "GET", &format!("/v1/work/{id}"), None);
                // A finished work may already have been evicted under the retention limit.
                if status == 410 {
                    break;
                }
                assert_eq!(status, 200);
                let v: Value = serde_json::from_str(&body).unwrap();
                if v["status"] != "running" && v["status"] != "queued" {
                    break;
                }
                assert!(Instant::now() < deadline, "work {id} never finished");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        ids.extend(wave);
    };
    run_wave(20, &mut ids);
    let warm = ids.len();
    let mut samples = vec![sample("after warm-up", warm)];
    let step = (works / 4).max(1);
    let mut done = 0;
    while done < works {
        let n = step.min(works - done);
        // Waves of up to 24 keep within the default queue limit (2 running + 32 queued).
        let mut left = n;
        while left > 0 {
            let w = left.min(24);
            run_wave(w, &mut ids);
            left -= w;
        }
        done += n;
        samples.push(sample(&format!("after {done} more"), ids.len()));
    }

    // What one finished item looks like to a client, and whether the first is still held.
    let probe = &ids[ids.len() - 1];
    let (_, result_body) = http(&addr, "GET", &format!("/v1/work/{probe}"), None);
    let (_, events_body) = http(&addr, "GET", &format!("/v1/work/{probe}/events"), None);
    let events: Value = serde_json::from_str(&events_body).unwrap();
    let (first_status, first_body) = http(&addr, "GET", &format!("/v1/work/{}", ids[0]), None);
    let first_code: Value = serde_json::from_str(&first_body).unwrap_or(Value::Null);
    let (_, health) = http(&addr, "GET", "/v1/metrics", None);

    let first = samples[0]["rss_anon_kib"].as_f64().unwrap();
    let last = samples[samples.len() - 1]["rss_anon_kib"].as_f64().unwrap();
    let retained = (ids.len() - warm) as f64;
    let out = json!({
        "scenario": scenario, "works_after_warmup": works, "file_kib": file_kib,
        "max_retained_work": max_retained,
        "retained_series": samples.iter().map(|s| s["retained_work"].clone()).collect::<Vec<_>>(),
        "first_work_lookup": {"status": first_status, "error_code": first_code["error"]["code"]},
        "samples": samples,
        "rss_anon_growth_kib": last - first,
        "bytes_retained_per_finished_work": (last - first) * 1024.0 / retained.max(1.0),
        "one_result_json_bytes": result_body.len(),
        "one_events_json_bytes": events_body.len(),
        "one_events_count": events["events"].as_array().map_or(0, Vec::len),
        "first_work_still_retrievable_after_all": first_status == 200,
        "metrics_excerpt": health.chars().take(400).collect::<String>(),
    });
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let works: usize = get("--works").and_then(|v| v.parse().ok()).unwrap_or(2000);
    let file_kib: usize = get("--file-kib").and_then(|v| v.parse().ok()).unwrap_or(32);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let bin = PathBuf::from(
        get("--bin")
            .or_else(|| std::env::var("CHIP_BIN").ok())
            .unwrap_or_else(|| root.join("release/chip").to_string_lossy().into_owned()),
    );
    assert!(
        bin.exists(),
        "build the binary first: cargo build --release -p chip-cli ({bin:?})"
    );
    let out_path = PathBuf::from(get("--out").unwrap_or_else(|| {
        root.join("service-retention.json")
            .to_string_lossy()
            .into_owned()
    }));
    // `--limits` are retention limits to run each scenario under. 1000000 is the largest the
    // service accepts and stands in for the old unbounded behavior.
    let limits: Vec<usize> = get("--limits")
        .unwrap_or_else(|| "64,1000000".into())
        .split(',')
        .map(|v| {
            v.parse()
                .expect("--limits is a comma-separated list of numbers")
        })
        .collect();
    let mut scenarios = Vec::new();
    for s in ["escape", "read"] {
        for &limit in &limits {
            let r = run_scenario(&bin, s, works, file_kib, limit);
            println!(
                "{s} limit {limit}: {} works, retained {:?}, anon RSS growth {:.0} KiB; first work lookup {} {}; result {} B, {} events {} B",
                works,
                r["retained_series"],
                r["rss_anon_growth_kib"].as_f64().unwrap(),
                r["first_work_lookup"]["status"],
                r["first_work_lookup"]["error_code"],
                r["one_result_json_bytes"],
                r["one_events_count"],
                r["one_events_json_bytes"],
            );
            scenarios.push(r);
        }
    }
    let doc = json!({
        "schema": "chip.service-retention.v2",
        "method": "Real `chip serve` binary, mock model and PAX shim, bounded runs; server process RssAnon read from /proc/<pid>/status after each wave of finished work. Measures retention of finished work in Service.items, not model or PAX behavior.",
        "binary": bin.to_string_lossy(),
        "scenarios": scenarios,
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    println!("wrote {}", out_path.display());
}
