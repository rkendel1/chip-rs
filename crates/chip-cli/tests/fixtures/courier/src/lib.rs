//! courier: a small request-dispatch client.
//!
//! A [`Client`] resolves each request to a route, applies rate limiting and a circuit breaker,
//! executes it through a [`Transport`] with retries, and records the outcome in a journal.
//! Configuration is layered (defaults, file, environment) and parsed into typed sections.

pub mod cli;
pub mod client;
pub mod config;
pub mod error;
pub mod exec;
pub mod http;
pub mod metrics;
pub mod middleware;
pub mod routes;
pub mod state;
pub mod transport;
pub mod util;

pub use client::{Client, ClientBuilder};
pub use config::Config;
pub use error::{CourierError, ErrorKind};
pub use http::{Headers, Method, Request, Response, Status, Url};
pub use transport::Transport;
