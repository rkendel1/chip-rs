//! The WebAssembly ABI: the shared export surface (`fx-wasm-abi`) instantiated for
//! [`Session`]. This file compiles for `wasm32` only and is the only place in this crate that
//! opts in to the ABI exports. It contains no raw-pointer code of its own, and the module it
//! produces imports nothing.
#![allow(unsafe_code)]

use crate::{ABI_VERSION, PROTOCOL_VERSION, Session};

fx_wasm_abi::export_json_session_abi! {
    session: Session,
    new: |_flags| Session::new(),
    abi_version: ABI_VERSION,
    protocol_version: PROTOCOL_VERSION,
    magic: 0x4348_4950,
    exports: {
        abi_version: chip_abi_version,
        protocol_version: chip_protocol_version,
        alloc: chip_alloc,
        free: chip_free,
        session_new: chip_session_new,
        session_free: chip_session_free,
        begin: chip_begin,
        command: chip_command,
        output_ptr: chip_output_ptr,
    },
}
