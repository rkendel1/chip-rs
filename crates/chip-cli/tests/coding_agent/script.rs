//! A scripted model: *judgment* supplied by the test, never reality.
//!
//! Every reply is a work decision in `chip.work-decision.v1` form; Chip parses and validates it
//! exactly as it would a real model's. A write reply carries the complete new content of a file;
//! the content is produced from an edit script applied to the file as it is on disk when the
//! reply is made, which stands in for a model that read the file and wrote its change back. The
//! script never supplies an execution id, an observation, a receipt or a result.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

pub enum Reply {
    Raw(String),
    /// Produced when the model is called, from the project as it is then.
    Write {
        path: String,
        hunks: Vec<Hunk>,
        new_file: Option<String>,
    },
}

#[derive(Clone)]
pub struct Hunk {
    pub find: String,
    pub replace: String,
}

pub struct ScriptedModel {
    dir: PathBuf,
    replies: Mutex<VecDeque<Reply>>,
    seen: Mutex<Vec<String>>,
    produced: Mutex<Vec<String>>,
}

impl ScriptedModel {
    pub fn new(dir: &Path, replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            dir: dir.to_path_buf(),
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
            produced: Mutex::new(Vec::new()),
        })
    }

    /// Everything this model was sent, one entry per call.
    pub fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    /// Every reply this model produced.
    pub fn produced(&self) -> Vec<String> {
        self.produced.lock().unwrap().clone()
    }

    pub fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    pub fn unused(&self) -> usize {
        self.replies.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedModel {
    async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.seen.lock().unwrap().push(
            r.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| FxError::Provider("the script ran out of replies".into()))?;
        let text = match reply {
            Reply::Raw(t) => t,
            Reply::Write {
                path,
                hunks,
                new_file,
            } => {
                let content = match new_file {
                    Some(c) => c,
                    None => {
                        let mut text = std::fs::read_to_string(self.dir.join(&path))
                            .unwrap_or_else(|e| panic!("script: cannot read {path}: {e}"));
                        for h in &hunks {
                            let n = text.matches(&h.find).count();
                            assert_eq!(
                                n, 1,
                                "script drift: {path}: the text to replace occurs {n} times:\n{}",
                                h.find
                            );
                            text = text.replacen(&h.find, &h.replace, 1);
                        }
                        text
                    }
                };
                write_reply(&path, &content)
            }
        };
        self.produced.lock().unwrap().push(text.clone());
        Ok(ModelResponse::new("scripted", text, Usage::new(1, 1)))
    }
}

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn request(capability: &str, inputs: &str) -> String {
    if inputs.is_empty() {
        format!(r#"{{"decision":"request_capability","capability":"{capability}"}}"#)
    } else {
        format!(
            r#"{{"decision":"request_capability","capability":"{capability}","inputs":{inputs}}}"#
        )
    }
}

pub fn list(path: &str) -> Reply {
    Reply::Raw(request(
        "project.list",
        &format!(r#"{{"path":{}}}"#, q(path)),
    ))
}
pub fn search(query: &str) -> Reply {
    Reply::Raw(request(
        "project.search",
        &format!(r#"{{"query":{}}}"#, q(query)),
    ))
}
pub fn read(path: &str) -> Reply {
    Reply::Raw(request(
        "project.read",
        &format!(r#"{{"path":{}}}"#, q(path)),
    ))
}
pub fn pax_test() -> Reply {
    Reply::Raw(request("pax.test", ""))
}
pub fn escalate(reason: &str) -> Reply {
    Reply::Raw(format!(
        r#"{{"decision":"escalate","reason":{}}}"#,
        q(reason)
    ))
}
pub fn complete(summary: &str) -> Reply {
    Reply::Raw(format!(
        r#"{{"decision":"complete","summary":{}}}"#,
        q(summary)
    ))
}
pub fn write_reply(path: &str, content: &str) -> String {
    request(
        "project.write",
        &format!(r#"{{"path":{},"content":{}}}"#, q(path), q(content)),
    )
}
/// An edit script: `@@@@ EDIT path` / `@@@@ FIND` / `@@@@ REPLACE` / `@@@@ END` blocks for
/// changes to an existing file, and `@@@@ NEW path` ... `@@@@ END` for a new file.
pub fn parse_edits(script: &str) -> Vec<Reply> {
    let mut order: Vec<String> = Vec::new();
    let mut hunks: std::collections::BTreeMap<String, Vec<Hunk>> = Default::default();
    let mut new_files: std::collections::BTreeMap<String, String> = Default::default();
    let mut lines = script.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if let Some(path) = line.strip_prefix("@@@@ NEW ") {
            let path = path.trim_end().to_string();
            let mut body = String::new();
            for l in lines.by_ref() {
                if l == "@@@@ END\n" {
                    break;
                }
                body.push_str(l);
            }
            order.push(path.clone());
            new_files.insert(path, body);
        } else if let Some(path) = line.strip_prefix("@@@@ EDIT ") {
            let path = path.trim_end().to_string();
            assert_eq!(lines.next(), Some("@@@@ FIND\n"), "malformed edit script");
            let (mut find, mut replace) = (String::new(), String::new());
            let mut in_replace = false;
            for l in lines.by_ref() {
                if l == "@@@@ REPLACE\n" {
                    in_replace = true;
                } else if l == "@@@@ END\n" {
                    break;
                } else if in_replace {
                    replace.push_str(l);
                } else {
                    find.push_str(l);
                }
            }
            if !order.contains(&path) {
                order.push(path.clone());
            }
            hunks.entry(path).or_default().push(Hunk { find, replace });
        } else if !line.trim().is_empty() {
            panic!("malformed edit script line: {line:?}");
        }
    }
    order
        .into_iter()
        .map(|path| Reply::Write {
            new_file: new_files.remove(&path),
            hunks: hunks.remove(&path).unwrap_or_default(),
            path,
        })
        .collect()
}

pub fn edits(name: &str) -> Vec<Reply> {
    let path = format!(
        "{}/tests/coding_agent/script/{name}.edits",
        env!("CARGO_MANIFEST_DIR")
    );
    parse_edits(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}
