//! # chip-fx
//!
//! Chip's model port ([`chip_inference::ModelProvider`]) served by the **authoritative FX**
//! inference executor from `rkendel1/fx` (`fx-core`, `fx-single-attempt`). It is the only
//! place Chip and FX meet, and it is an adapter, not a second executor:
//!
//! - [`mapping`] is pure. It turns a Chip [`chip_inference::ModelRequest`] into the FX
//!   request document, and an FX outcome back into a Chip response or a failure that
//!   keeps the executor's delivery certainty. It has no I/O and compiles for `wasm32`.
//! - [`report`] reads the JSON execution report a host (for example the WebAssembly FX session)
//!   returns, through FX's own decoder, and applies the same mapping.
//! - [`FxModelProvider`] drives `fx_core::execute` over `fx_single_attempt` for an
//!   embedder that supplies a [`fx_single_attempt::ProviderPort`]. Every inference call is
//!   one FX attempt: no retry, no fallback, no routing, no second provider call.
//!
//! What this crate does not do: invoke a provider itself, validate a request (FX does),
//! normalize a stream (FX does), decide about tools or authorization (Chip does), or
//! retry. A failed or ambiguous attempt is returned as
//! [`chip_inference::FxError::Inference`] with its delivery certainty; Chip's work loop
//! ends the work as failed and never repeats the call.
//!
//! See `docs/product/authoritative-fx.md`.

pub mod mapping;
mod native;
pub mod report;

pub use mapping::{OptionsPolicy, RequestIds, from_outcome, kind_label, to_fx_request};
pub use native::{
    CancelSignal, FxModelProvider, Never, ProviderStats, current_call_cancel, with_call_cancel,
};
