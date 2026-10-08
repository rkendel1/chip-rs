//! Policies applied around the executor: request preparation, admission control and logging.

pub mod auth;
pub mod circuit;
pub mod headers;
pub mod inflight;
pub mod logging;
pub mod rate_limit;

use crate::config::Config;
use crate::http::Request;

/// Something that adjusts a request before it is sent.
pub trait Prepare: Send + Sync {
    fn prepare(&self, request: &mut Request);
}

/// The preparers a configuration asks for, in the order they run.
pub fn preparers(config: &Config) -> Vec<Box<dyn Prepare>> {
    let mut list: Vec<Box<dyn Prepare>> = vec![Box::new(headers::DefaultHeaders::new(
        &config.client.user_agent,
    ))];
    if let Some(token) = &config.auth.token {
        list.push(Box::new(auth::Authorization::new(&config.auth.scheme, token)));
    }
    list
}
