//! Small, dependency-free helpers.

pub mod clock;
pub mod duration;
pub mod id;
pub mod rng;
pub mod text;

pub use clock::{Clock, ManualClock, SystemClock};
