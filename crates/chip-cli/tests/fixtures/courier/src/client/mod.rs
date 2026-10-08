//! The public entry point: [`Client`] sends requests; [`ClientBuilder`] assembles one from a
//! [`Config`](crate::Config).

pub mod builder;
pub mod session;

pub use builder::ClientBuilder;
pub use session::Session;

use crate::config::Config;
use crate::error::{CourierError, ErrorKind};
use crate::exec::Executor;
use crate::http::{Request, Response, Url};
use crate::metrics::Registry;
use crate::middleware::circuit::{CircuitBreaker, State};
use crate::middleware::inflight::InflightGate;
use crate::middleware::logging::EventLog;
use crate::middleware::rate_limit::RateLimiter;
use crate::middleware::Prepare;
use crate::routes::{Resolved, RouteOverrides, RouteTable};
use crate::state::journal::{Entry, Journal, Outcome};
use crate::state::{snapshot, Store};
use crate::util::Clock;
use std::sync::{Arc, Mutex};

pub struct Client {
    pub(crate) config: Config,
    pub(crate) routes: Mutex<RouteTable>,
    pub(crate) executor: Executor,
    pub(crate) preparers: Vec<Box<dyn Prepare>>,
    pub(crate) limiter: RateLimiter,
    pub(crate) breaker: CircuitBreaker,
    pub(crate) gate: InflightGate,
    pub(crate) journal: Option<Journal>,
    pub(crate) store: Option<Store>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) metrics: Arc<Registry>,
    pub(crate) log: EventLog,
}

impl Client {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn metrics(&self) -> &Registry {
        &self.metrics
    }

    pub fn log_lines(&self) -> Vec<String> {
        self.log.lines()
    }

    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }

    pub fn executor(&self) -> &Executor {
        &self.executor
    }

    pub fn route_for(&self, url: &Url) -> Resolved {
        self.routes.lock().unwrap().resolve(url)
    }

    pub fn breaker_state(&self, route: &str) -> State {
        self.breaker.state(route)
    }

    /// Replaces a route's overrides; later requests on it use the new values.
    pub fn set_route_overrides(&self, route: &str, overrides: RouteOverrides) -> bool {
        self.routes.lock().unwrap().set_overrides(route, overrides)
    }

    pub fn get(&self, url: &str) -> Result<Response, CourierError> {
        self.send(Request::get(url)?)
    }

    pub fn send(&self, mut request: Request) -> Result<Response, CourierError> {
        for p in &self.preparers {
            p.prepare(&mut request);
        }
        let resolved = self.route_for(&request.url);
        let route = resolved.route.as_str();
        self.metrics.counter("requests").inc();

        let _permit = match self.gate.try_enter(resolved.settings.max_inflight) {
            Some(p) => p,
            None => return self.reject(&request, route, ErrorKind::RateLimited, "too many requests in flight"),
        };
        if !self.limiter.try_acquire() {
            return self.reject(&request, route, ErrorKind::RateLimited, "rate limit exceeded");
        }
        if !self.breaker.admit(route) {
            return self.reject(&request, route, ErrorKind::CircuitOpen, "circuit is open");
        }

        let started = self.clock.now();
        let result = self.executor.execute(&request, &resolved.settings);
        self.metrics.latency.observe(self.clock.now().saturating_sub(started));

        match &result {
            Ok(_) => self.breaker.record_success(route),
            Err(e) => match e.kind {
                ErrorKind::RetriesExhausted
                | ErrorKind::Transport
                | ErrorKind::WaitTooLong
                | ErrorKind::DeadlineExceeded => self.breaker.record_failure(route),
                _ => self.breaker.record_success(route),
            },
        }
        self.metrics
            .counter(if result.is_ok() { "successes" } else { "failures" })
            .inc();
        self.finish(&request, route, &result);
        result
    }

    fn reject(
        &self,
        request: &Request,
        route: &str,
        kind: ErrorKind,
        message: &str,
    ) -> Result<Response, CourierError> {
        self.metrics.counter("rejected").inc();
        let err = CourierError::new(kind, message);
        self.record(request, route, 0, Outcome::Rejected, message);
        self.log.push(format!("{} {} rejected: {message}", request.method, request.url));
        Err(err)
    }

    fn finish(&self, request: &Request, route: &str, result: &Result<Response, CourierError>) {
        let (attempts, outcome, detail) = match result {
            Ok(r) => (1, Outcome::Success, r.status.to_string()),
            Err(e) => (e.attempts, Outcome::Failure, e.to_string()),
        };
        self.log.push(format!(
            "{} {} -> {}",
            request.method,
            request.url,
            if outcome == Outcome::Success { "ok" } else { "failed" }
        ));
        self.record(request, route, attempts, outcome, &detail);
        if let Some(store) = &self.store {
            if let Err(e) = snapshot::save(store, &self.breaker.snapshot()) {
                self.log.push(format!("could not save circuit state: {e}"));
            }
        }
    }

    fn record(&self, request: &Request, route: &str, attempts: u32, outcome: Outcome, detail: &str) {
        if let Some(journal) = &self.journal {
            let entry = Entry {
                time_ms: self.clock.now().as_millis() as u64,
                route: route.to_string(),
                method: request.method.to_string(),
                url: request.url.to_string(),
                attempts,
                outcome,
                detail: detail.to_string(),
            };
            if let Err(e) = journal.append(&entry) {
                self.log.push(format!("could not write the journal: {e}"));
            }
        }
    }
}
