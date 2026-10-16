use std::collections::BTreeMap;

use chip_core::{ExecutionStatus, InputValue, WorkDecision, WorkOutcome, WorkReport};
use chip_session::{
    ExecutionMemory, History, HistoryTurn, HistoryTurnCommitted, SessionError, TerminalReason,
    ToolCall, ToolResult, ToolStatus, ToolStep, UserTurn,
};

fn arguments_json(inputs: &BTreeMap<String, InputValue>) -> Result<String, SessionError> {
    let map: serde_json::Map<String, serde_json::Value> = inputs
        .iter()
        .map(|(key, value)| {
            let json = match value {
                InputValue::Text(text) => serde_json::Value::String(text.clone()),
                InputValue::Integer(number) => serde_json::Value::from(*number),
                InputValue::Bool(flag) => serde_json::Value::Bool(*flag),
            };
            (key.clone(), json)
        })
        .collect();
    serde_json::to_string(&map).map_err(|e| SessionError::InvalidFrame {
        offset: 0,
        reason: e.to_string(),
    })
}

/// The committed turn a finished work report stands for, in the existing Zig shape.
///
/// * `Completed` becomes an `assistant` turn whose answer is the summary Chip verified.
/// * Every other outcome becomes an `interrupted` turn (`failed` for `Failed`, `cancelled` for the
///   rest). The Zig format has no field for Chip's reason text, so it is not carried; the work
///   report remains the only record of it.
/// * Each authorized execution becomes one tool step: the call (Chip's execution id as the call id,
///   the capability as the tool name, its inputs as JSON) and its result. The model's text decisions
///   are not recorded as assistant text.
///
/// Token totals are cumulative, as a commit replaces the session's totals.
pub fn turn_from_report(
    history: &History,
    prompt: &str,
    report: &WorkReport,
    now_ms: i64,
) -> Result<HistoryTurnCommitted, SessionError> {
    let mut requests: BTreeMap<String, (&str, &BTreeMap<String, InputValue>)> = BTreeMap::new();
    for record in &report.decisions {
        if let WorkDecision::RequestCapability(request) = &record.decision {
            requests.insert(
                request.execution_id.0.clone(),
                (request.capability_id.as_str(), &request.inputs),
            );
        }
    }
    let mut steps = Vec::new();
    let mut names = Vec::new();
    for (observation, origin) in report.observations.iter().zip(&report.origins) {
        let id = observation.execution_id.0.clone();
        let (name, arguments) = match requests.get(&id) {
            Some((name, inputs)) => ((*name).to_string(), arguments_json(inputs)?),
            None => (origin.capability.as_str().to_string(), "{}".to_string()),
        };
        let status = match observation.status {
            ExecutionStatus::Success => ToolStatus::Success,
            ExecutionStatus::Failure | ExecutionStatus::Cancelled => ToolStatus::Failure,
        };
        let call = ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments_json: arguments,
            provider_result: None,
        };
        let result = ToolResult::new(
            id,
            name.clone(),
            status,
            observation.output.clone().unwrap_or_default(),
            now_ms,
        );
        names.push(name);
        steps.push(ToolStep {
            assistant: None,
            tool_calls: vec![call],
            tool_results: vec![result],
        });
    }
    let execution = ExecutionMemory::new(steps, Vec::new());
    let user = UserTurn::text(prompt);
    let turn = match &report.outcome {
        WorkOutcome::Completed { summary } => HistoryTurn::Assistant {
            user,
            assistant: summary.clone(),
            execution,
        },
        outcome => HistoryTurn::Interrupted {
            user,
            assistant: None,
            tool_call: None,
            completed_tool_names: names,
            execution,
            terminal_reason: if matches!(outcome, WorkOutcome::Failed { .. }) {
                TerminalReason::Failed
            } else {
                TerminalReason::Cancelled
            },
        },
    };
    Ok(HistoryTurnCommitted {
        conversation_language: history.conversation_language.clone(),
        total_input_tokens: history.total_input_tokens + report.summary.prompt_tokens,
        total_output_tokens: history.total_output_tokens + report.summary.completion_tokens,
        work_id: None,
        turn,
    })
}
