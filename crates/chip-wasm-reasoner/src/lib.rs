//! Host for an untrusted WASM module that implements Chip's `LocalReasoner`.
//!
//! The module is a calculator for one judgment. It gets no imports, so it has no
//! filesystem, process, network, environment, clock or host-state access: a module
//! that declares any import is refused. Each call uses a fresh instance with a
//! fixed fuel budget, so results are repeatable and CPU is bounded.
//!
//! ABI (version 1), all integers little-endian:
//! - the module exports `memory`, `input_offset() -> i32` and `reason(len: i32) -> i32`;
//! - the host writes the encoded `ReasoningInput` at `input_offset()` and calls
//!   `reason(len)`, which returns the offset of the encoded verdict;
//! - input: `[version=1][evidence: 0 valid|1 stale|2 unknown][u16 n][capability]
//!   [u16 count]{[u16 n][name][tag: 0 text u16 n+bytes | 1 i64 | 2 u8 bool]}*`;
//! - verdict: `[tag: 0 continue|1 escalate][u16 n][utf-8 text]`.

use chip_core::{
    EvidenceState, InputValue, LocalReasoner, LocalReasoningResult, ReasoningError, ReasoningInput,
};
use wasmi::{Config, Engine, Linker, Module, Store};

pub const ABI_VERSION: u8 = 1;
const MAX_INPUT_BYTES: usize = 8192;
const MAX_VERDICT_TEXT: usize = 256;
const FUEL: u64 = 1_000_000;

fn failed(message: impl Into<String>) -> ReasoningError {
    ReasoningError::Failed(message.into())
}

/// Serializes a `ReasoningInput` for the module.
pub fn encode_input(input: &ReasoningInput) -> Result<Vec<u8>, ReasoningError> {
    fn put_str(out: &mut Vec<u8>, text: &str) -> Result<(), ReasoningError> {
        let len = u16::try_from(text.len()).map_err(|_| failed("text too long to encode"))?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        Ok(())
    }
    let mut out = vec![
        ABI_VERSION,
        match input.evidence {
            EvidenceState::KnownValid => 0,
            EvidenceState::KnownStale => 1,
            EvidenceState::Unknown => 2,
        },
    ];
    put_str(&mut out, input.capability.as_str())?;
    let count = u16::try_from(input.inputs.len()).map_err(|_| failed("too many inputs"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for (name, value) in &input.inputs {
        put_str(&mut out, name)?;
        match value {
            InputValue::Text(text) => {
                out.push(0);
                put_str(&mut out, text)?;
            }
            InputValue::Integer(number) => {
                out.push(1);
                out.extend_from_slice(&number.to_le_bytes());
            }
            InputValue::Bool(flag) => {
                out.push(2);
                out.push(u8::from(*flag));
            }
        }
    }
    if out.len() > MAX_INPUT_BYTES {
        return Err(failed("encoded input exceeds the size limit"));
    }
    Ok(out)
}

/// Parses the module's verdict.
pub fn decode_verdict(bytes: &[u8]) -> Result<LocalReasoningResult, ReasoningError> {
    let [tag, lo, hi, text @ ..] = bytes else {
        return Err(failed("verdict is too short"));
    };
    let len = u16::from_le_bytes([*lo, *hi]) as usize;
    if len > MAX_VERDICT_TEXT || text.len() < len {
        return Err(failed("verdict text length is invalid"));
    }
    let text =
        String::from_utf8(text[..len].to_vec()).map_err(|_| failed("verdict is not utf-8"))?;
    match tag {
        0 => Ok(LocalReasoningResult::Continue { rationale: text }),
        1 => Ok(LocalReasoningResult::Escalate { reason: text }),
        _ => Err(failed("unknown verdict tag")),
    }
}

/// A `LocalReasoner` backed by a WASM module.
pub struct WasmLocalReasoner {
    engine: Engine,
    module: Module,
}

impl WasmLocalReasoner {
    /// Compiles and validates a module. A module that declares any import is refused.
    pub fn from_bytes(wasm: &[u8]) -> Result<Self, ReasoningError> {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module =
            Module::new(&engine, wasm).map_err(|e| failed(format!("invalid module: {e}")))?;
        if let Some(import) = module.imports().next() {
            return Err(failed(format!(
                "module imports `{}::{}`; reasoning modules get no host access",
                import.module(),
                import.name()
            )));
        }
        Ok(Self { engine, module })
    }
}

impl LocalReasoner for WasmLocalReasoner {
    fn reason(&self, input: &ReasoningInput) -> Result<LocalReasoningResult, ReasoningError> {
        let encoded = encode_input(input)?;
        let mut store = Store::new(&self.engine, ());
        store
            .set_fuel(FUEL)
            .map_err(|e| failed(format!("fuel: {e}")))?;
        let instance = Linker::<()>::new(&self.engine)
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| failed(format!("instantiation: {e}")))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| failed("module exports no memory"))?;
        let input_offset = instance
            .get_typed_func::<(), i32>(&store, "input_offset")
            .map_err(|e| failed(format!("input_offset: {e}")))?
            .call(&mut store, ())
            .map_err(|e| failed(format!("input_offset trapped: {e}")))?;
        let input_offset = usize::try_from(input_offset).map_err(|_| failed("bad input offset"))?;
        memory
            .write(&mut store, input_offset, &encoded)
            .map_err(|_| failed("input does not fit the module's memory"))?;
        let verdict_offset = instance
            .get_typed_func::<i32, i32>(&store, "reason")
            .map_err(|e| failed(format!("reason: {e}")))?
            .call(&mut store, encoded.len() as i32)
            .map_err(|e| failed(format!("reason trapped: {e}")))?;
        let verdict_offset =
            usize::try_from(verdict_offset).map_err(|_| failed("bad verdict offset"))?;
        let mut header = [0u8; 3];
        memory
            .read(&store, verdict_offset, &mut header)
            .map_err(|_| failed("verdict is outside module memory"))?;
        let len = u16::from_le_bytes([header[1], header[2]]) as usize;
        let mut bytes = vec![0u8; 3 + len.min(MAX_VERDICT_TEXT + 1)];
        memory
            .read(&store, verdict_offset, &mut bytes)
            .map_err(|_| failed("verdict is outside module memory"))?;
        decode_verdict(&bytes)
    }
}
