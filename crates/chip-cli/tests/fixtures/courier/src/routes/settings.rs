//! Per-route overrides of the client's execution settings.

use crate::config::RouteSection;
use crate::exec::ExecSettings;
use std::time::Duration;

/// What one route changes. A field left as `None` keeps the client's value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteOverrides {
    pub timeout: Option<Duration>,
    pub deadline: Option<Duration>,
    pub max_wait: Option<Duration>,
    pub max_inflight: Option<usize>,
}

impl RouteOverrides {
    /// The settings a request on this route is executed with.
    pub fn apply(&self, base: &ExecSettings) -> ExecSettings {
        ExecSettings {
            timeout: self.timeout.unwrap_or(base.timeout),
            deadline: self.deadline.or(base.deadline),
            max_wait: self.max_wait.or(base.max_wait),
            max_inflight: self.max_inflight.unwrap_or(base.max_inflight),
            ..ExecSettings::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == RouteOverrides::default()
    }
}

impl From<&RouteSection> for RouteOverrides {
    fn from(r: &RouteSection) -> Self {
        Self {
            timeout: r.timeout,
            deadline: r.deadline,
            max_wait: r.max_wait,
            max_inflight: r.max_inflight,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ExecSettings {
        ExecSettings {
            timeout: Duration::from_secs(4),
            deadline: Some(Duration::from_secs(40)),
            max_wait: Some(Duration::from_secs(8)),
            max_inflight: 6,
            ..ExecSettings::default()
        }
    }

    #[test]
    fn empty_overrides_keep_the_base() {
        assert_eq!(RouteOverrides::default().apply(&base()), base());
    }

    #[test]
    fn set_fields_replace_and_unset_fields_inherit() {
        let o = RouteOverrides {
            timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        };
        let s = o.apply(&base());
        assert_eq!(s.timeout, Duration::from_secs(30));
        assert_eq!(s.deadline, Some(Duration::from_secs(40)));
        assert_eq!(s.max_inflight, 6);
    }

    #[test]
    fn built_from_a_config_section() {
        let section = RouteSection {
            name: "r".into(),
            prefix: "/r".into(),
            timeout: Some(Duration::from_secs(1)),
            deadline: None,
            max_wait: None,
            max_inflight: Some(2),
        };
        let o = RouteOverrides::from(&section);
        assert_eq!(o.timeout, Some(Duration::from_secs(1)));
        assert_eq!(o.max_inflight, Some(2));
        assert!(!o.is_empty());
        assert!(RouteOverrides::default().is_empty());
    }
}
