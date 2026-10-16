//! Test doubles for the host side of the boundary: a provider port that serves one scripted
//! attempt per FX attempt, and a work environment with one declared capability. Neither
//! bypasses Chip or FX: they sit where the host would, behind `ProviderPort` and behind
//! Chip's `Executor`.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chip_core::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId, CapabilityInput,
    CapabilityProvider, ExecutionError, ExecutionRequest, ExecutionResult, Executor, WorkGoal,
    WorkId, WorkSpec,
};
use fx_core::{AttemptBudget, Request};
use fx_single_attempt::testing::{done, poll};
use fx_single_attempt::{
    AbortReport, Observed, PortFailure, PortFailureKind, PortItem, PortPoll, PortSupport,
    ProviderPort, Terminal,
};

/// What one FX attempt should look like to the port.
#[derive(Clone, Debug)]
pub enum Attempt {
    /// A complete reply: the text, then a provider-backed `done` with finish `stop`.
    Reply(String),
    /// The call itself fails before any output.
    Fail(PortFailure),
    /// Some text, then the stream ends without a terminal marker.
    Cut(String),
    /// A reply whose poll flips the shared flag, as a caller cancelling mid-call would.
    CancelWhileStreaming(String),
}

/// A `ProviderPort` serving one scripted attempt per `invoke`, in order. It records every
/// request it was handed and every call it received, so tests can count provider invocations.
pub struct SequencePort {
    attempts: VecDeque<Attempt>,
    pub requests: Arc<Mutex<Vec<Request>>>,
    pub invokes: Arc<AtomicUsize>,
    pub aborts: Arc<AtomicUsize>,
    support: PortSupport,
    cancel_flag: Option<Arc<AtomicBool>>,
}

/// The per-call state.
pub struct SequenceCall {
    polls: VecDeque<Result<PortPoll, PortFailure>>,
    flip: Option<Arc<AtomicBool>>,
}

impl SequencePort {
    pub fn new(attempts: Vec<Attempt>) -> Self {
        SequencePort {
            attempts: attempts.into(),
            requests: Arc::new(Mutex::new(Vec::new())),
            invokes: Arc::new(AtomicUsize::new(0)),
            aborts: Arc::new(AtomicUsize::new(0)),
            support: PortSupport::default(),
            cancel_flag: None,
        }
    }

    pub fn with_cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel_flag = Some(flag);
        self
    }

    pub fn with_support(mut self, support: PortSupport) -> Self {
        self.support = support;
        self
    }
}

pub fn failure(kind: PortFailureKind, observed: Observed, status: Option<u16>) -> PortFailure {
    PortFailure {
        kind,
        message: "scripted".to_owned(),
        observed,
        status,
        retry_after: None,
    }
}

fn reply_items(text: &str) -> Vec<PortItem> {
    vec![
        PortItem::Text(text.to_owned()),
        done(Some("stop"), Terminal::ProviderMarker),
    ]
}

impl ProviderPort for SequencePort {
    type Call = SequenceCall;

    fn support(&self) -> PortSupport {
        self.support
    }

    fn invoke(
        &mut self,
        request: &Request,
        _budget: Option<AttemptBudget>,
    ) -> Result<SequenceCall, PortFailure> {
        self.invokes.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request.clone());
        let attempt = self
            .attempts
            .pop_front()
            .expect("the port was invoked more times than the test scripted");
        match attempt {
            Attempt::Fail(failure) => Err(failure),
            Attempt::Reply(text) => Ok(SequenceCall {
                polls: VecDeque::from([Ok(poll(
                    reply_items(&text),
                    Observed::OutputObserved,
                    true,
                ))]),
                flip: None,
            }),
            Attempt::Cut(text) => Ok(SequenceCall {
                polls: VecDeque::from([Ok(poll(
                    vec![PortItem::Text(text)],
                    Observed::OutputObserved,
                    true,
                ))]),
                flip: None,
            }),
            Attempt::CancelWhileStreaming(text) => Ok(SequenceCall {
                polls: VecDeque::from([
                    Ok(poll(
                        vec![PortItem::Text(text)],
                        Observed::OutputObserved,
                        false,
                    )),
                    Ok(poll(Vec::new(), Observed::OutputObserved, false)),
                    Ok(poll(
                        vec![done(Some("stop"), Terminal::ProviderMarker)],
                        Observed::OutputObserved,
                        true,
                    )),
                ]),
                flip: self.cancel_flag.clone(),
            }),
        }
    }

    fn poll(
        &mut self,
        call: &mut SequenceCall,
        _max_items: usize,
    ) -> Result<PortPoll, PortFailure> {
        let next = call
            .polls
            .pop_front()
            .unwrap_or_else(|| Ok(poll(Vec::new(), Observed::Invoked, true)));
        // The caller cancels while the stream is open: after the first batch was delivered.
        if let Some(flag) = &call.flip
            && call.polls.len() < 2
        {
            flag.store(true, Ordering::SeqCst);
        }
        next
    }

    fn abort(&mut self, _call: &mut SequenceCall) -> AbortReport {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        AbortReport {
            transport_closed: false,
        }
    }

    fn release(&mut self, _call: SequenceCall) {}
}

/// The environment: one capability, `compute.selftest`, and an executor that counts runs.
pub struct Caps;

#[async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut descriptor = CapabilityDescriptor::new(
            CapabilityId::new("compute.selftest")?,
            "selftest",
            "runs the self test",
        );
        descriptor.inputs.push(CapabilityInput {
            name: "n".into(),
            description: String::new(),
            required: false,
        });
        Ok(vec![descriptor])
    }

    async fn availability(&self, _: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

/// Counts how often Chip executed something, and can be made to fail.
pub struct CountingExecutor {
    pub runs: AtomicUsize,
    pub fail: bool,
}

impl CountingExecutor {
    pub fn new() -> Arc<CountingExecutor> {
        Arc::new(CountingExecutor {
            runs: AtomicUsize::new(0),
            fail: false,
        })
    }

    pub fn failing() -> Arc<CountingExecutor> {
        Arc::new(CountingExecutor {
            runs: AtomicUsize::new(0),
            fail: true,
        })
    }

    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Ok(ExecutionResult::failure(request.id, "self test failed"));
        }
        Ok(ExecutionResult::success(request.id, "self test passed")
            .with_receipt_id("sha256:receipt"))
    }
}

pub fn spec() -> WorkSpec {
    WorkSpec::new(
        WorkId::new("w"),
        WorkGoal::new("Run the self test and report the result"),
    )
}

pub const REQUEST_SELFTEST: &str =
    r#"{"decision":"request_capability","capability":"compute.selftest"}"#;
pub const COMPLETE: &str = r#"{"decision":"complete","summary":"the self test passed"}"#;
