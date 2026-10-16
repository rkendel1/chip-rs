//! # chip-wasm-host
//!
//! Chip's work loop (`chip-core`) as a **host-driven session**, in the same shape as FX's
//! host session (`fx-host`): bytes in, a JSON response out, driven by a host that owns every
//! effect. The module can run a work, but it can reach nothing:
//!
//! - **Inference** is not performed here. When the loop needs a model decision this session
//!   builds the FX request (`chip-fx`) and answers `need_inference`. The host runs it
//!   through the authoritative FX session and Easy-LLM, and returns FX's execution report.
//! - **Execution** is not performed here. When Chip has validated a capability request and
//!   constructed an execution, the session answers `need_execution`. The host's explicit
//!   capability runs it and reports the result, which becomes an observation.
//! - There is no clock, filesystem, process, environment or network access, no thread and no
//!   async runtime: the work future is polled by hand, and it suspends only on the two
//!   answers above.
//!
//! The protocol is documented in `docs/product/authoritative-fx.md`. Authorization,
//! validation of the model's decision, execution identity, observation, evidence and
//! termination are `chip-core`'s, unchanged. Nothing here decides anything.

#[cfg(target_arch = "wasm32")]
mod abi;
mod session;

pub use session::{PROTOCOL_VERSION, Session};

/// The WebAssembly ABI version exported as `chip_abi_version`.
pub const ABI_VERSION: u32 = 1;

impl fx_wasm_abi::JsonSession for Session {
    fn begin(&mut self, input: &[u8]) -> Vec<u8> {
        Session::begin(self, input)
    }

    fn command(&mut self, input: &[u8]) -> Vec<u8> {
        Session::command(self, input)
    }
}
