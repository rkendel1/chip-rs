//! A tiny typed decision module.
//!
//! It consumes the exact canonical `CapabilityDecisionState` bytes (`chip.decision.v1`) and
//! returns `Continue` or `Escalate`. There is no text generation, no JSON, no allocation, no
//! filesystem, no network, no process, no threads, no imports and no dependencies.
//!
//! On Wasm the crate is `#![no_std]`. Natively it builds with `std` so it works as an ordinary
//! library and as the reference oracle; the code itself uses only `core`, and is the same code
//! in both builds.
//!
//! Invalid input never becomes `Continue`: anything that is not a well-formed state is reported
//! as [`abi::INVALID`], which the host must map to an error.
#![cfg_attr(target_arch = "wasm32", no_std)]

pub mod abi;
pub mod model;

pub use abi::{ABI_VERSION, AbiError, CONTINUE, ESCALATE, INVALID};
pub use model::{Decision, DecisionResult, decide_state};

/// Decides from canonical state bytes. The one function both builds share.
pub fn decide_bytes(canonical: &[u8]) -> Result<DecisionResult, AbiError> {
    let state = abi::parse(canonical)?;
    Ok(decide_state(state.evidence, state.impact))
}

/// The raw result code for canonical bytes: [`CONTINUE`], [`ESCALATE`] or [`INVALID`].
pub fn decide_code(canonical: &[u8]) -> u8 {
    match decide_bytes(canonical) {
        Ok(result) => result.decision.code(),
        Err(_) => INVALID,
    }
}

#[cfg(target_arch = "wasm32")]
mod exports {
    use super::{INVALID, abi, decide_code};

    const INPUT_CAPACITY: usize = abi::MAX_STATE_BYTES;
    const OUTPUT_CAPACITY: usize = 8;

    // Single-threaded Wasm: the module owns its buffers, so the host needs no allocator call.
    static mut INPUT: [u8; INPUT_CAPACITY] = [0; INPUT_CAPACITY];
    static mut OUTPUT: [u8; OUTPUT_CAPACITY] = [0; OUTPUT_CAPACITY];

    /// Where the host writes the canonical state bytes.
    #[unsafe(no_mangle)]
    pub extern "C" fn chip_input_ptr() -> *mut u8 {
        (&raw mut INPUT).cast()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn chip_input_capacity() -> usize {
        INPUT_CAPACITY
    }

    /// Where the result code is written.
    #[unsafe(no_mangle)]
    pub extern "C" fn chip_output_ptr() -> *mut u8 {
        (&raw mut OUTPUT).cast()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn chip_output_capacity() -> usize {
        OUTPUT_CAPACITY
    }

    /// The ABI version string (`chip.decision.v1`), as bytes in module memory.
    #[unsafe(no_mangle)]
    pub extern "C" fn chip_abi_version_ptr() -> *const u8 {
        abi::ABI_VERSION.as_ptr()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn chip_abi_version_len() -> usize {
        abi::ABI_VERSION.len()
    }

    /// Decides from `input_len` bytes at `input_ptr`, writing one result code (0 continue,
    /// 1 escalate, 255 invalid) at `output_ptr`. Returns the number of bytes written: 1, or 0
    /// when the output buffer or pointers are unusable (which the host treats as invalid).
    ///
    /// # Safety
    /// `input_ptr`/`output_ptr` must point to `input_len`/`output_len` bytes of this module's
    /// memory. The host passes the buffers this module exports.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn chip_decide(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_len: usize,
    ) -> usize {
        if output_ptr.is_null() || output_len == 0 {
            return 0;
        }
        let code = if input_ptr.is_null() || input_len > abi::MAX_STATE_BYTES {
            INVALID
        } else {
            // SAFETY: the caller guarantees the range; length is bounded above.
            decide_code(unsafe { core::slice::from_raw_parts(input_ptr, input_len) })
        };
        // SAFETY: the caller guarantees `output_len >= 1` bytes at `output_ptr`.
        unsafe { output_ptr.write(code) };
        1
    }

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo) -> ! {
        core::arch::wasm32::unreachable()
    }
}
