//! Wraps a transport and injects failures on a deterministic schedule.

use super::{Transport, TransportError, TransportErrorKind};
use crate::http::{Request, Response};
use crate::util::rng::Rng;
use std::sync::Mutex;
use std::time::Duration;

pub struct FaultTransport<T: Transport> {
    inner: T,
    every: u64,
    calls: Mutex<u64>,
    rng: Mutex<Rng>,
}

impl<T: Transport> FaultTransport<T> {
    /// Fails every `every`-th call (0 disables faults). The kind of fault is drawn from a seeded
    /// generator, so a run is reproducible.
    pub fn new(inner: T, every: u64, seed: u64) -> Self {
        Self {
            inner,
            every,
            calls: Mutex::new(0),
            rng: Mutex::new(Rng::seeded(seed)),
        }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T: Transport> Transport for FaultTransport<T> {
    fn send(&self, request: &Request, timeout: Duration) -> Result<Response, TransportError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        if self.every > 0 && n % self.every == 0 {
            let kind = match self.rng.lock().unwrap().below(3) {
                0 => TransportErrorKind::Connect,
                1 => TransportErrorKind::Timeout,
                _ => TransportErrorKind::Reset,
            };
            return Err(TransportError::new(kind, "injected fault"));
        }
        self.inner.send(request, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::{ok, MockTransport};

    fn req() -> Request {
        Request::get("http://h/").unwrap()
    }

    #[test]
    fn faults_land_on_the_schedule() {
        let t = FaultTransport::new(MockTransport::new(vec![ok(), ok(), ok(), ok()]), 3, 1);
        let d = Duration::from_secs(1);
        assert!(t.send(&req(), d).is_ok());
        assert!(t.send(&req(), d).is_ok());
        assert!(t.send(&req(), d).is_err());
        assert!(t.send(&req(), d).is_ok());
    }

    #[test]
    fn zero_disables_faults() {
        let t = FaultTransport::new(MockTransport::new(vec![ok(), ok()]), 0, 1);
        let d = Duration::from_secs(1);
        assert!(t.send(&req(), d).is_ok());
        assert!(t.send(&req(), d).is_ok());
        assert_eq!(t.inner().calls(), 2);
    }

    #[test]
    fn fault_kinds_are_reproducible() {
        let kinds = |seed| {
            let t = FaultTransport::new(MockTransport::new(vec![]), 1, seed);
            (0..6)
                .map(|_| t.send(&req(), Duration::from_secs(1)).unwrap_err().kind)
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(5), kinds(5));
    }
}
