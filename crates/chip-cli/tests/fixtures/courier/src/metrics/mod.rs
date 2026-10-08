//! Counters and a latency histogram, rendered as plain `name value` lines.

pub mod counter;
pub mod histogram;
pub mod registry;

pub use counter::Counter;
pub use histogram::Histogram;
pub use registry::Registry;
