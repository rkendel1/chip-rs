//! One `ProviderPort` over the selected native provider. It only forwards: every rule (one
//! request, no retry, delivery certainty, cancellation) lives in `fx-native-provider`.

use fx_core::{AttemptBudget, Request};
use fx_native_provider::{Anthropic, NativeCall, OpenAiCompatible};
use fx_single_attempt::{AbortReport, PortFailure, PortPoll, PortSupport, ProviderPort};

pub(crate) enum NativePort {
    OpenAi(OpenAiCompatible),
    Anthropic(Anthropic),
}

impl ProviderPort for NativePort {
    type Call = NativeCall;

    fn support(&self) -> PortSupport {
        match self {
            Self::OpenAi(p) => p.support(),
            Self::Anthropic(p) => p.support(),
        }
    }

    fn invoke(
        &mut self,
        request: &Request,
        budget: Option<AttemptBudget>,
    ) -> Result<NativeCall, PortFailure> {
        match self {
            Self::OpenAi(p) => p.invoke(request, budget),
            Self::Anthropic(p) => p.invoke(request, budget),
        }
    }

    fn poll(&mut self, call: &mut NativeCall, max_items: usize) -> Result<PortPoll, PortFailure> {
        match self {
            Self::OpenAi(p) => p.poll(call, max_items),
            Self::Anthropic(p) => p.poll(call, max_items),
        }
    }

    fn abort(&mut self, call: &mut NativeCall) -> AbortReport {
        match self {
            Self::OpenAi(p) => p.abort(call),
            Self::Anthropic(p) => p.abort(call),
        }
    }

    fn release(&mut self, call: NativeCall) {
        match self {
            Self::OpenAi(p) => p.release(call),
            Self::Anthropic(p) => p.release(call),
        }
    }
}
