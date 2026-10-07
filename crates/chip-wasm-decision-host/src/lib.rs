//! Host for the typed Wasm decision module.
//!
//! The module is a calculator for one judgment: it gets no imports, so no filesystem,
//! network, process, clock or host-state access; a module that declares any import is refused.
//! The host keeps one instance alive and calls it repeatedly, because creating an instance is
//! far more expensive than a decision. Every call runs under a fixed fuel budget.
//!
//! Safety invariant: invalid state can never become permission to continue. The module
//! answers `255` for anything that is not a well-formed state, and the host maps that, any
//! other unknown code, a short write, a trap or an exhausted budget to an error.

use std::fmt;

use chip_core::{CapabilityDecisionState, EvidenceState, ImpactState};
use chip_wasm_decision::abi::{self, ABI_VERSION};
pub use chip_wasm_decision::{Decision, DecisionResult};
use wasmi::{Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

const FUEL: u64 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionError {
    /// The module could not be compiled, instantiated or does not meet the contract.
    Module(String),
    /// The module's ABI version is not `chip.decision.v1`.
    IncompatibleAbi(String),
    /// The module declared an import; decision modules get no host access.
    ForbiddenImport(String),
    /// The module reported the input as invalid (result code 255).
    InvalidState,
    /// The module returned something other than a defined result.
    BadOutput(String),
    /// The call trapped or ran out of fuel.
    Trapped(String),
}

impl fmt::Display for DecisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecisionError::Module(m) => write!(f, "decision module: {m}"),
            DecisionError::IncompatibleAbi(v) => {
                write!(
                    f,
                    "incompatible decision ABI: expected {ABI_VERSION}, module says {v:?}"
                )
            }
            DecisionError::ForbiddenImport(i) => {
                write!(
                    f,
                    "decision module imports `{i}`; decision modules get no host access"
                )
            }
            DecisionError::InvalidState => {
                f.write_str("the module rejected the decision state as invalid")
            }
            DecisionError::BadOutput(m) => write!(f, "decision module returned a bad output: {m}"),
            DecisionError::Trapped(m) => write!(f, "decision module trapped: {m}"),
        }
    }
}

impl std::error::Error for DecisionError {}

fn module_error(what: &str, e: impl fmt::Display) -> DecisionError {
    DecisionError::Module(format!("{what}: {e}"))
}

/// The native reference: the same policy written directly over the typed state, independent
/// of the byte parser and of Wasm. It is the correctness oracle for both.
pub fn decide_native(state: &CapabilityDecisionState) -> DecisionResult {
    DecisionResult::certain(match (state.evidence_state, state.impact) {
        (EvidenceState::KnownValid, ImpactState::Unchanged) => Decision::Continue,
        _ => Decision::Escalate,
    })
}

/// A compiled, validated module. Cheap to share; instantiate it to decide.
pub struct WasmDecisionModule {
    engine: Engine,
    module: Module,
}

impl WasmDecisionModule {
    /// Compiles a module and refuses any that declares an import.
    pub fn compile(wasm: &[u8]) -> Result<Self, DecisionError> {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).map_err(|e| module_error("invalid module", e))?;
        if let Some(import) = module.imports().next() {
            return Err(DecisionError::ForbiddenImport(format!(
                "{}::{}",
                import.module(),
                import.name()
            )));
        }
        Ok(Self { engine, module })
    }

    /// Creates a live instance, verifies the ABI version and resolves the module's buffers.
    pub fn instantiate(&self) -> Result<WasmDecisionEngine, DecisionError> {
        let mut store = Store::new(&self.engine, ());
        store.set_fuel(FUEL).map_err(|e| module_error("fuel", e))?;
        let instance = Linker::<()>::new(&self.engine)
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| module_error("instantiation", e))?;
        WasmDecisionEngine::bind(store, instance)
    }
}

/// A live instance. Keep it alive and call [`decide`](Self::decide) repeatedly.
pub struct WasmDecisionEngine {
    store: Store<()>,
    memory: Memory,
    decide: TypedFunc<(i32, i32, i32, i32), i32>,
    input_ptr: usize,
    input_capacity: usize,
    output_ptr: usize,
    output_capacity: usize,
    scratch: Vec<u8>,
}

impl WasmDecisionEngine {
    /// Compiles and instantiates in one step.
    pub fn new(wasm: &[u8]) -> Result<Self, DecisionError> {
        WasmDecisionModule::compile(wasm)?.instantiate()
    }

    fn bind(mut store: Store<()>, instance: Instance) -> Result<Self, DecisionError> {
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| DecisionError::Module("module exports no memory".into()))?;
        let mut number = |name: &str| -> Result<usize, DecisionError> {
            let value = instance
                .get_typed_func::<(), i32>(&store, name)
                .map_err(|e| module_error(name, e))?
                .call(&mut store, ())
                .map_err(|e| module_error(name, e))?;
            usize::try_from(value).map_err(|_| module_error(name, "negative"))
        };
        let version_ptr = number("chip_abi_version_ptr")?;
        let version_len = number("chip_abi_version_len")?;
        let input_ptr = number("chip_input_ptr")?;
        let input_capacity = number("chip_input_capacity")?;
        let output_ptr = number("chip_output_ptr")?;
        let output_capacity = number("chip_output_capacity")?;

        let memory_len = memory.data_size(&store);
        for (what, ptr, len) in [
            ("abi version", version_ptr, version_len),
            ("input buffer", input_ptr, input_capacity),
            ("output buffer", output_ptr, output_capacity),
        ] {
            if ptr.checked_add(len).is_none_or(|end| end > memory_len) {
                return Err(DecisionError::Module(format!(
                    "{what} lies outside module memory"
                )));
            }
        }
        if input_capacity < abi::MAX_STATE_BYTES || output_capacity == 0 {
            return Err(DecisionError::Module("module buffers are too small".into()));
        }
        let mut version = vec![0u8; version_len.min(64)];
        memory
            .read(&store, version_ptr, &mut version)
            .map_err(|e| module_error("abi version", e))?;
        if version != ABI_VERSION.as_bytes() {
            return Err(DecisionError::IncompatibleAbi(
                String::from_utf8_lossy(&version).into_owned(),
            ));
        }

        let decide = instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&store, "chip_decide")
            .map_err(|e| module_error("chip_decide", e))?;
        Ok(Self {
            store,
            memory,
            decide,
            input_ptr,
            input_capacity,
            output_ptr,
            output_capacity,
            scratch: Vec::with_capacity(256),
        })
    }

    /// Decides from a typed state: encodes it into a reused buffer, then calls the module.
    pub fn decide(
        &mut self,
        state: &CapabilityDecisionState,
    ) -> Result<DecisionResult, DecisionError> {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        state.write_canonical(&mut scratch);
        let result = self.decide_bytes(&scratch);
        self.scratch = scratch;
        result
    }

    /// Decides from canonical state bytes, exactly as `chip-core` encodes them.
    pub fn decide_bytes(&mut self, canonical: &[u8]) -> Result<DecisionResult, DecisionError> {
        if canonical.len() > self.input_capacity {
            return Err(DecisionError::InvalidState);
        }
        self.store
            .set_fuel(FUEL)
            .map_err(|e| DecisionError::Trapped(e.to_string()))?;
        self.memory
            .write(&mut self.store, self.input_ptr, canonical)
            .map_err(|e| module_error("writing input", e))?;
        let written = self
            .decide
            .call(
                &mut self.store,
                (
                    self.input_ptr as i32,
                    canonical.len() as i32,
                    self.output_ptr as i32,
                    self.output_capacity as i32,
                ),
            )
            .map_err(|e| DecisionError::Trapped(e.to_string()))?;
        if written != 1 {
            return Err(DecisionError::BadOutput(format!(
                "{written} bytes written, expected 1"
            )));
        }
        let mut code = [0u8; 1];
        self.memory
            .read(&self.store, self.output_ptr, &mut code)
            .map_err(|e| module_error("reading output", e))?;
        match code[0] {
            abi::INVALID => Err(DecisionError::InvalidState),
            other => Decision::from_code(other)
                .map(DecisionResult::certain)
                .ok_or_else(|| DecisionError::BadOutput(format!("unknown result code {other}"))),
        }
    }
}
