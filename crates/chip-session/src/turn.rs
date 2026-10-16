//! The turn events of the Zig session format, as Rust types.
//!
//! These mirror `session_codec.zig` (`parseHistoryTurn`, `parseUserTurn`, `parseExecutionMemory`,
//! `parseToolStep`, `parseToolResult`, `parseFile`) and `session_event.zig`
//! (`history_turn_committed`). Decoding is as strict as Zig's: exact key sets, nullable-but-required
//! members must be present, no duplicates. Encoding writes the same key order Zig writes.
//!
//! Only execution-memory schema 3 is interpreted; schemas 1 and 2 are refused as `Unsupported`
//! rather than reinterpreted. Turn kinds `background_command` and `compacted_summary` are likewise
//! refused: a Rust consumer must not silently skip history it does not understand.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

use crate::SessionError;

/// A key that must be present even when its value is `null` (Zig's `object.get(key) orelse error`).
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// JSON carried verbatim, byte for byte, without interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawJson(pub String);

impl Serialize for RawJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        RawValue::from_string(self.0.clone())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RawJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(
            Box::<RawValue>::deserialize(deserializer)?
                .get()
                .to_string(),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageAttachment {
    pub id: u64,
    pub path: String,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_sha256: Option<String>,
}

/// What the user asked. `work_id` is present only for delegated work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserTurn {
    pub text: String,
    pub images: Vec<ImageAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
}

impl UserTurn {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
            work_id: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
    #[serde(deserialize_with = "present")]
    pub provider_result: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Success,
    Failure,
}

/// One tool result (schema 3). The three presentation members are carried as opaque JSON: Rust
/// writes `null` and preserves anything Zig wrote without interpreting it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    pub output: String,
    #[serde(deserialize_with = "present")]
    pub output_handle: Option<String>,
    #[serde(deserialize_with = "present")]
    pub preview: Option<String>,
    pub output_bytes: u64,
    pub stored_output_bytes: u64,
    pub truncated: bool,
    pub provider_native: bool,
    pub created_at_ms: i64,
    pub permission_feedback: Vec<String>,
    #[serde(deserialize_with = "present")]
    pub committed_file_presentation: Option<RawJson>,
    #[serde(deserialize_with = "present")]
    pub command_output_replay: Option<RawJson>,
    #[serde(deserialize_with = "present")]
    pub command_process_presentation: Option<RawJson>,
}

impl ToolResult {
    /// A result as Rust records it: the output is stored whole and nothing is truncated.
    pub fn new(
        tool_call_id: String,
        tool_name: String,
        status: ToolStatus,
        output: String,
        created_at_ms: i64,
    ) -> Self {
        let bytes = output.len() as u64;
        Self {
            tool_call_id,
            tool_name,
            status,
            output,
            output_handle: None,
            preview: None,
            output_bytes: bytes,
            stored_output_bytes: bytes,
            truncated: false,
            provider_native: false,
            created_at_ms,
            permission_feedback: Vec::new(),
            committed_file_presentation: None,
            command_output_replay: None,
            command_process_presentation: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolStep {
    #[serde(deserialize_with = "present")]
    pub assistant: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAction {
    Read,
    Write,
    Edit,
    Delete,
    Rename,
    Copy,
    Search,
    List,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEvidence {
    pub path: String,
    #[serde(deserialize_with = "present")]
    pub new_path: Option<String>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub action: FileAction,
    pub status: ToolStatus,
    pub model_view_covers_full_file: bool,
    pub stale: bool,
}

/// `execution`: what the tools did during the turn. Always schema 3 here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionMemory {
    pub schema_version: u64,
    pub tool_steps: Vec<ToolStep>,
    pub files: Vec<FileEvidence>,
}

pub const EXECUTION_SCHEMA: u64 = 3;

impl ExecutionMemory {
    pub fn new(tool_steps: Vec<ToolStep>, files: Vec<FileEvidence>) -> Self {
        Self {
            schema_version: EXECUTION_SCHEMA,
            tool_steps,
            files,
        }
    }

    fn check(&self) -> Result<(), SessionError> {
        if self.schema_version != EXECUTION_SCHEMA {
            return Err(SessionError::Unsupported(format!(
                "execution memory schema {} (only schema {EXECUTION_SCHEMA} is interpreted)",
                self.schema_version
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    Cancelled,
    Failed,
}

/// One committed turn of history.
#[derive(Clone, Debug, PartialEq)]
pub enum HistoryTurn {
    /// The turn completed with an assistant answer.
    Assistant {
        user: UserTurn,
        assistant: String,
        execution: ExecutionMemory,
    },
    /// The turn ended without an answer (cancelled or failed).
    Interrupted {
        user: UserTurn,
        assistant: Option<String>,
        tool_call: Option<ToolCall>,
        completed_tool_names: Vec<String>,
        execution: ExecutionMemory,
        terminal_reason: TerminalReason,
    },
}

#[derive(Deserialize)]
struct KindPeek {
    kind: Option<String>,
}

#[derive(Deserialize)]
struct SchemaPeek {
    schema_version: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssistantWire {
    #[allow(dead_code)]
    kind: String,
    user: UserTurn,
    assistant: String,
    execution: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InterruptedWire {
    #[allow(dead_code)]
    kind: String,
    user: UserTurn,
    #[serde(deserialize_with = "present")]
    assistant: Option<String>,
    #[serde(deserialize_with = "present")]
    tool_call: Option<ToolCall>,
    completed_tool_names: Vec<String>,
    #[serde(default)]
    execution: Option<Box<RawValue>>,
    #[serde(default)]
    cancelled_command: Option<Box<RawValue>>,
    #[serde(default)]
    terminal_reason: Option<TerminalReason>,
}

#[derive(Serialize)]
struct AssistantOut<'a> {
    kind: &'static str,
    user: &'a UserTurn,
    assistant: &'a str,
    execution: &'a ExecutionMemory,
}

#[derive(Serialize)]
struct InterruptedOut<'a> {
    kind: &'static str,
    user: &'a UserTurn,
    assistant: &'a Option<String>,
    tool_call: &'a Option<ToolCall>,
    completed_tool_names: &'a [String],
    execution: &'a ExecutionMemory,
    terminal_reason: TerminalReason,
}

fn bad(reason: impl ToString) -> SessionError {
    SessionError::InvalidFormat {
        file: "history_turn_committed".into(),
        reason: reason.to_string(),
    }
}

fn decode_execution(raw: &RawValue) -> Result<ExecutionMemory, SessionError> {
    // The schema number decides how the rest is read; check it first so an older schema is reported
    // as unsupported rather than as a malformed schema-3 object.
    let peek: SchemaPeek = serde_json::from_str(raw.get()).map_err(bad)?;
    match peek.schema_version {
        Some(EXECUTION_SCHEMA) => {}
        Some(other @ (1 | 2)) => {
            return Err(SessionError::Unsupported(format!(
                "execution memory schema {other} (only schema {EXECUTION_SCHEMA} is interpreted)"
            )));
        }
        _ => return Err(bad("execution.schema_version is not 1, 2 or 3")),
    }
    serde_json::from_str(raw.get()).map_err(bad)
}

impl HistoryTurn {
    pub fn user(&self) -> &UserTurn {
        match self {
            Self::Assistant { user, .. } | Self::Interrupted { user, .. } => user,
        }
    }

    pub fn execution(&self) -> &ExecutionMemory {
        match self {
            Self::Assistant { execution, .. } | Self::Interrupted { execution, .. } => execution,
        }
    }

    pub fn from_json(json: &str) -> Result<Self, SessionError> {
        let kind = serde_json::from_str::<KindPeek>(json)
            .map_err(bad)?
            .kind
            .ok_or_else(|| bad("turn.kind is missing"))?;
        match kind.as_str() {
            "assistant" => {
                let wire: AssistantWire = serde_json::from_str(json).map_err(bad)?;
                Ok(Self::Assistant {
                    user: wire.user,
                    assistant: wire.assistant,
                    execution: decode_execution(&wire.execution)?,
                })
            }
            "interrupted" => {
                let wire: InterruptedWire = serde_json::from_str(json).map_err(bad)?;
                if wire.cancelled_command.is_some() {
                    return Err(SessionError::Unsupported(
                        "interrupted turn with a cancelled_command presentation".into(),
                    ));
                }
                let execution = match &wire.execution {
                    Some(raw) => decode_execution(raw)?,
                    None => ExecutionMemory::new(Vec::new(), Vec::new()),
                };
                Ok(Self::Interrupted {
                    user: wire.user,
                    assistant: wire.assistant,
                    tool_call: wire.tool_call,
                    completed_tool_names: wire.completed_tool_names,
                    execution,
                    terminal_reason: wire.terminal_reason.unwrap_or(TerminalReason::Cancelled),
                })
            }
            "background_command" | "compacted_summary" => Err(SessionError::Unsupported(format!(
                "history turn kind {kind}"
            ))),
            other => Err(bad(format!("unknown turn kind {other}"))),
        }
    }

    /// The turn JSON in Zig's key order.
    pub fn to_json(&self) -> Result<String, SessionError> {
        self.execution().check()?;
        let json = match self {
            Self::Assistant {
                user,
                assistant,
                execution,
            } => serde_json::to_string(&AssistantOut {
                kind: "assistant",
                user,
                assistant,
                execution,
            }),
            Self::Interrupted {
                user,
                assistant,
                tool_call,
                completed_tool_names,
                execution,
                terminal_reason,
            } => serde_json::to_string(&InterruptedOut {
                kind: "interrupted",
                user,
                assistant,
                tool_call,
                completed_tool_names,
                execution,
                terminal_reason: *terminal_reason,
            }),
        };
        json.map_err(bad)
    }
}

/// The payload of a `history_turn_committed` event. The token totals are cumulative for the
/// session: a commit replaces the session's totals, it does not add to them.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryTurnCommitted {
    pub conversation_language: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub work_id: Option<String>,
    pub turn: HistoryTurn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommittedWire {
    conversation_language: String,
    total_input_tokens: u64,
    total_output_tokens: u64,
    turn: Box<RawValue>,
    #[serde(default)]
    work_id: Option<String>,
}

impl HistoryTurnCommitted {
    pub fn from_payload_json(json: &str) -> Result<Self, SessionError> {
        let wire: CommittedWire = serde_json::from_str(json).map_err(bad)?;
        Ok(Self {
            conversation_language: wire.conversation_language,
            total_input_tokens: wire.total_input_tokens,
            total_output_tokens: wire.total_output_tokens,
            work_id: wire.work_id,
            turn: HistoryTurn::from_json(wire.turn.get())?,
        })
    }

    /// The payload JSON in Zig's key order.
    pub fn to_payload_json(&self) -> Result<String, SessionError> {
        let mut out = String::from("{");
        out.push_str(&format!(
            "\"conversation_language\":{},",
            serde_json::to_string(&self.conversation_language).map_err(bad)?
        ));
        out.push_str(&format!(
            "\"total_input_tokens\":{},\"total_output_tokens\":{},",
            self.total_input_tokens, self.total_output_tokens
        ));
        out.push_str(&format!("\"turn\":{}", self.turn.to_json()?));
        if let Some(work_id) = &self.work_id {
            out.push_str(&format!(
                ",\"work_id\":{}",
                serde_json::to_string(work_id).map_err(bad)?
            ));
        }
        out.push('}');
        Ok(out)
    }
}
