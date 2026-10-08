//! Routing: which settings apply to which request.

pub mod matcher;
pub mod settings;
pub mod table;

pub use settings::RouteOverrides;
pub use table::{Resolved, RouteTable};
