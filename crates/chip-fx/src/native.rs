//! The native driver: Chip's model port over `fx_core::execute` and a `ProviderPort`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chip_inference::{FxError, ModelProvider, ModelRequest, ModelResponse};
use fx_core::{Host, Limits, ResponseFormat, execute};
use fx_single_attempt::{ProviderPort, SingleAttemptCapability};

use crate::mapping::{OptionsPolicy, RequestIds, from_outcome, to_fx_request};

/// A cooperative cancellation signal the embedder owns. FX polls it before the provider is
/// invoked and between polls; it cannot interrupt a provider call that is in progress.
pub trait CancelSignal: Send + Sync {
    /// Whether the caller asked to cancel.
    fn cancelled(&self) -> bool;
}

/// A signal that never fires.
#[derive(Clone, Copy, Debug, Default)]
pub struct Never;

impl CancelSignal for Never {
    fn cancelled(&self) -> bool {
        false
    }
}

struct CancelHost<'a>(&'a dyn CancelSignal);

impl Host for CancelHost<'_> {
    fn cancel_requested(&mut self) -> bool {
        self.0.cancelled()
    }
}

/// Counters for audit. They are facts about this provider, never control inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderStats {
    /// FX attempts started (one per `complete`).
    pub attempts: u64,
    /// Requests whose sampling options were dropped under [`OptionsPolicy::DropUnsupported`].
    pub options_dropped: u64,
}

/// Chip's model port over the authoritative FX executor.
///
/// Each [`ModelProvider::complete`] call is exactly one FX attempt. The embedder supplies
/// the [`ProviderPort`] (the host's explicit provider capability); this type never opens a
/// connection, reads the environment or chooses a provider.
pub struct FxModelProvider<P: ProviderPort> {
    capability: Mutex<SingleAttemptCapability<P>>,
    ids: Mutex<RequestIds>,
    stats: Mutex<ProviderStats>,
    policy: OptionsPolicy,
    budget: Option<Duration>,
    cancel: Arc<dyn CancelSignal>,
    limits: Limits,
    response_format: Option<ResponseFormat>,
}

impl<P: ProviderPort> FxModelProvider<P> {
    /// A provider over `port`. `policy` has no default: the embedder decides what happens
    /// to the sampling options FX cannot carry.
    pub fn new(port: P, ids: RequestIds, policy: OptionsPolicy) -> Self {
        FxModelProvider {
            capability: Mutex::new(SingleAttemptCapability::new(port)),
            ids: Mutex::new(ids),
            stats: Mutex::new(ProviderStats::default()),
            policy,
            budget: None,
            cancel: Arc::new(Never),
            limits: Limits::default(),
            response_format: None,
        }
    }

    /// Sets the per-attempt budget handed to FX.
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Sets the cancellation signal FX polls.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Arc<dyn CancelSignal>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Asks for one JSON object on every request, as Chip's work loop does. FX carries it as a
    /// structured-output request (a schema of `{"type":"object"}`); a port that cannot carry
    /// structured output refuses the request before any provider call.
    #[must_use]
    pub fn with_json_object_output(mut self) -> Self {
        self.response_format = Some(ResponseFormat {
            name: "chip_reply".to_owned(),
            description: "One JSON object".to_owned(),
            schema_json: r#"{"type":"object"}"#.to_owned(),
        });
        self
    }

    /// The counters so far.
    pub fn stats(&self) -> ProviderStats {
        *self.stats.lock().expect("stats lock")
    }

    /// Runs `inspect` on the port (for example to read a test double's observations).
    pub fn with_port<R>(&self, inspect: impl FnOnce(&P) -> R) -> R {
        inspect(self.capability.lock().expect("capability lock").port())
    }
}

#[async_trait]
impl<P> ModelProvider for FxModelProvider<P>
where
    P: ProviderPort + Send,
    P::Call: Send,
{
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        let id = self.ids.lock().expect("ids lock").issue();
        {
            let mut stats = self.stats.lock().expect("stats lock");
            stats.attempts += 1;
            if self.policy == OptionsPolicy::DropUnsupported
                && (request.max_tokens.is_some() || request.temperature.is_some())
            {
                stats.options_dropped += 1;
            }
        }
        let mut fx_request = to_fx_request(&request, id, self.policy, self.budget)?;
        fx_request.options.response_format = self.response_format.clone();
        let mut host = CancelHost(self.cancel.as_ref());
        let report = {
            let mut capability = self.capability.lock().expect("capability lock");
            execute(&mut *capability, &fx_request, &self.limits, &mut host)
        };
        from_outcome(&report.outcome, report.disposition, id)
    }
}
