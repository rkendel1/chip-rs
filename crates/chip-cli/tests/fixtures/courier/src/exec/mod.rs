//! Request execution: one logical request becomes one or more attempts against a transport.

pub mod attempt;
pub mod backoff;
pub mod classify;
pub mod deadline;
pub mod executor;
pub mod retry;
pub mod settings;

pub use backoff::Backoff;
pub use classify::{classify, Disposition, Failure};
pub use executor::Executor;
pub use retry::Retry;
pub use settings::ExecSettings;
