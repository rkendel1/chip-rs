//! Runs one request to completion: attempts, waits and the decision to give up.

use super::attempt::AttemptState;
use super::backoff::Backoff;
use super::classify::{classify, Disposition, Failure};
use super::deadline::Deadline;
use super::retry::Retry;
use super::settings::ExecSettings;
use crate::error::{CourierError, ErrorKind};
use crate::http::{Request, Response};
use crate::transport::Transport;
use crate::util::Clock;
use std::sync::Arc;
use std::time::Duration;

pub struct Executor {
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    retry: Retry,
    backoff: Backoff,
}

impl Executor {
    pub fn new(transport: Arc<dyn Transport>, clock: Arc<dyn Clock>) -> Self {
        Self {
            transport,
            clock,
            retry: Retry::standard(),
            backoff: Backoff::standard(),
        }
    }

    pub fn retry(&self) -> Retry {
        self.retry
    }

    /// Sends `request`, retrying failures that are worth retrying.
    pub fn execute(
        &self,
        request: &Request,
        settings: &ExecSettings,
    ) -> Result<Response, CourierError> {
        let started = self.clock.now();
        let deadline = Deadline::new(started, settings.deadline);
        let mut state = AttemptState::new();
        loop {
            state.begin();
            let outcome = self.transport.send(request, settings.timeout);
            let failure = match outcome {
                Ok(response) if response.status.is_success() || response.status.is_redirection() => {
                    return Ok(response)
                }
                Ok(response) => Failure::Status(response.status, response.retry_after()),
                Err(e) => Failure::Transport(e.kind, e.message),
            };
            state.record(&failure);
            if classify(&failure) == Disposition::Terminal {
                return Err(failure.into_error(ErrorKind::Status, state.made()));
            }
            let hint = failure.retry_after();
            if hint.is_some() {
                state.paused_for_server();
            }
            if !self.retry.permits(state.made()) {
                return Err(failure.into_error(ErrorKind::RetriesExhausted, state.made()));
            }
            let mut delay = self.backoff.delay(state.made());
            if let Some(hint) = hint {
                delay = delay.max(hint);
            }
            self.pause(delay, settings, &deadline, state.made())?;
        }
    }

    fn pause(
        &self,
        delay: Duration,
        settings: &ExecSettings,
        deadline: &Deadline,
        attempts: u32,
    ) -> Result<(), CourierError> {
        if let Some(max) = settings.max_wait {
            if delay > max {
                return Err(CourierError::new(
                    ErrorKind::WaitTooLong,
                    format!("a wait of {delay:?} exceeds max_wait {max:?}"),
                )
                .with_attempts(attempts));
            }
        }
        if deadline.forbids(self.clock.now(), delay) {
            return Err(CourierError::new(
                ErrorKind::DeadlineExceeded,
                format!("a wait of {delay:?} would pass the request deadline"),
            )
            .with_attempts(attempts));
        }
        self.clock.sleep(delay);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::{connect_error, ok, status, MockTransport};
    use crate::util::ManualClock;

    fn run(script: Vec<crate::transport::mock::Outcome>) -> (Result<Response, CourierError>, usize, Arc<ManualClock>) {
        let transport = Arc::new(MockTransport::new(script));
        let clock = Arc::new(ManualClock::new());
        let exec = Executor::new(transport.clone(), clock.clone());
        let r = exec.execute(&Request::get("http://h/").unwrap(), &ExecSettings::default());
        (r, transport.calls(), clock)
    }

    #[test]
    fn success_on_first_attempt_does_not_wait() {
        let (r, calls, clock) = run(vec![ok()]);
        assert!(r.is_ok());
        assert_eq!(calls, 1);
        assert!(clock.slept().is_empty());
    }

    #[test]
    fn retries_server_errors_until_success() {
        let (r, calls, clock) = run(vec![status(503), status(500), ok()]);
        assert!(r.is_ok());
        assert_eq!(calls, 3);
        assert_eq!(clock.slept(), vec![Duration::from_millis(100), Duration::from_millis(200)]);
    }

    #[test]
    fn gives_up_after_three_attempts() {
        let (r, calls, _) = run(vec![status(503), status(503), status(503), ok()]);
        let e = r.unwrap_err();
        assert_eq!(e.kind, ErrorKind::RetriesExhausted);
        assert_eq!(e.attempts, 3);
        assert_eq!(calls, 3);
    }

    #[test]
    fn terminal_statuses_are_not_retried() {
        let (r, calls, _) = run(vec![status(404), ok()]);
        let e = r.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Status);
        assert_eq!(calls, 1);
    }

    #[test]
    fn transport_errors_are_retried() {
        let (r, calls, _) = run(vec![connect_error(), ok()]);
        assert!(r.is_ok());
        assert_eq!(calls, 2);
    }

    #[test]
    fn redirections_are_returned_not_retried() {
        let (r, calls, _) = run(vec![status(302)]);
        assert_eq!(r.unwrap().status.code(), 302);
        assert_eq!(calls, 1);
    }

    #[test]
    fn deadline_stops_a_wait_that_would_overrun() {
        let transport = Arc::new(MockTransport::new(vec![status(503), ok()]));
        let clock = Arc::new(ManualClock::new());
        let exec = Executor::new(transport, clock.clone());
        let settings = ExecSettings {
            deadline: Some(Duration::from_millis(50)),
            ..ExecSettings::default()
        };
        let e = exec.execute(&Request::get("http://h/").unwrap(), &settings).unwrap_err();
        assert_eq!(e.kind, ErrorKind::DeadlineExceeded);
        assert!(clock.slept().is_empty());
    }
}
