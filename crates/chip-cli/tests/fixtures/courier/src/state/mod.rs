//! Persistent state: the request journal and the saved circuit-breaker snapshot.

pub mod codec;
pub mod journal;
pub mod snapshot;
pub mod store;

pub use journal::{Entry, Journal, Outcome};
pub use store::Store;
