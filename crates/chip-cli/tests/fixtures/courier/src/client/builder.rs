use super::Client;
use crate::config::{self, Config};
use crate::error::CourierError;
use crate::exec::Executor;
use crate::metrics::Registry;
use crate::middleware::circuit::CircuitBreaker;
use crate::middleware::inflight::InflightGate;
use crate::middleware::logging::EventLog;
use crate::middleware::rate_limit::RateLimiter;
use crate::middleware::preparers;
use crate::routes::RouteTable;
use crate::state::{snapshot, Journal, Store};
use crate::transport::Transport;
use crate::util::{Clock, SystemClock};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct ClientBuilder {
    config: Config,
    transport: Option<Arc<dyn Transport>>,
    clock: Option<Arc<dyn Clock>>,
}

impl ClientBuilder {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            transport: None,
            clock: None,
        }
    }

    /// Defaults, then the given configuration text, then the given environment pairs.
    pub fn from_text(text: &str, env: &[(String, String)]) -> Result<Self, CourierError> {
        Ok(Self::new(config::load(text, env)?.config))
    }

    pub fn transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    pub fn build(self) -> Result<Client, CourierError> {
        let transport = self
            .transport
            .ok_or_else(|| CourierError::config("a client needs a transport"))?;
        let clock: Arc<dyn Clock> = self.clock.unwrap_or_else(|| Arc::new(SystemClock::new()));
        let config = self.config;

        let (journal, store) = match (&config.journal.enabled, &config.journal.path) {
            (true, Some(path)) => {
                let path = PathBuf::from(path);
                let store = Store::new(&path.with_extension("state"));
                (Some(Journal::open(&path)?), Some(store))
            }
            _ => (None, None),
        };

        let breaker = CircuitBreaker::new(config.circuit.clone(), clock.clone());
        if let Some(store) = &store {
            breaker.restore(snapshot::load(store)?);
        }

        Ok(Client {
            routes: Mutex::new(RouteTable::from_config(&config)),
            executor: Executor::new(transport, clock.clone()),
            preparers: preparers(&config),
            limiter: RateLimiter::new(&config.rate_limit, clock.clone()),
            breaker,
            gate: InflightGate::new(),
            journal,
            store,
            clock,
            metrics: Arc::new(Registry::new()),
            log: EventLog::new(256),
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::{ok, MockTransport};

    #[test]
    fn a_transport_is_required() {
        let e = ClientBuilder::from_text("", &[]).unwrap().build().err().unwrap();
        assert!(e.message.contains("transport"));
    }

    #[test]
    fn builds_with_defaults() {
        let c = ClientBuilder::from_text("", &[])
            .unwrap()
            .transport(Arc::new(MockTransport::new(vec![ok()])))
            .build()
            .unwrap();
        assert!(c.get("http://h/").is_ok());
    }

    #[test]
    fn bad_configuration_never_builds() {
        assert!(ClientBuilder::from_text("[client]\ntimeout = 0ms\n", &[]).is_err());
    }
}
