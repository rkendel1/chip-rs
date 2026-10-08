//! Precedence between defaults, the configuration file and the environment.

use courier::config::{self, Layers, RawConfig};
use std::time::Duration;

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

#[test]
fn defaults_apply_when_nothing_else_is_set() {
    let c = config::load("", &[]).unwrap().config;
    assert_eq!(c.client.timeout, Duration::from_secs(10));
    assert_eq!(c.circuit.threshold, 5);
}

#[test]
fn the_file_overrides_defaults_key_by_key() {
    let c = config::load("[client]\ntimeout = 2s\n", &[]).unwrap().config;
    assert_eq!(c.client.timeout, Duration::from_secs(2));
    assert_eq!(c.client.max_inflight, 32, "keys the file does not set keep their defaults");
}

#[test]
fn the_environment_overrides_the_file() {
    let c = config::load("[client]\ntimeout = 2s\n", &env(&[("COURIER_CLIENT_TIMEOUT", "7s")]))
        .unwrap()
        .config;
    assert_eq!(c.client.timeout, Duration::from_secs(7));
}

#[test]
fn an_invalid_environment_value_is_an_error() {
    let e = config::load("", &env(&[("COURIER_CLIENT_TIMEOUT", "soon")])).unwrap_err();
    assert!(e.message.contains("timeout"));
}

#[test]
fn unrelated_environment_variables_are_ignored() {
    let loaded = config::load("", &env(&[("HOME", "/root"), ("COURIER_ELSEWHERE", "1")])).unwrap();
    assert!(loaded.warnings.is_empty());
}

#[test]
fn layers_report_where_a_value_came_from() {
    let mut l = Layers::new();
    let mut file = RawConfig::new();
    file.set("client", "timeout", "1s");
    l.push("defaults", config::defaults::default_raw());
    l.push("file", file);
    assert_eq!(l.origin("client", "timeout"), Some("file"));
    assert_eq!(l.origin("client", "max_inflight"), Some("defaults"));
}

#[test]
fn route_sections_come_from_the_file_only() {
    let c = config::load(
        "[route.reports]\nprefix = /reports\ntimeout = 30s\n",
        &env(&[("COURIER_ROUTE_REPORTS_TIMEOUT", "1s")]),
    )
    .unwrap()
    .config;
    assert_eq!(c.routes[0].timeout, Some(Duration::from_secs(30)));
}
