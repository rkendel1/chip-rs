//! What "done" means for the assignment, checked against the running program.
//!
//! The runtime's own completion rule for change work is "PAX passed after the last change". That
//! establishes that the tests pass; it cannot know what the assignment asked for. These checks
//! do: each one runs the compiled `courier` binary (or compares trees) and reports what happened.

use std::path::Path;

use super::reality::{self, Ran};

pub const OBJECTIVE: &str = "Add support for configurable retry policies to the request execution layer of this crate. Requirements: support bounded retries; preserve existing default behaviour; distinguish retryable from terminal failures; expose configuration through the existing public configuration path; stay backwards compatible; add unit coverage; add integration coverage; reject invalid configuration; update the relevant documentation. Preserve existing public behaviour unless a requirement changes it. Understand the existing architecture before modifying it. Do not weaken or delete tests to make the implementation pass. If you cannot establish that a change is correct, stop and escalate with the evidence you have.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub id: &'static str,
    pub description: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// A decision a human gave, with the behaviour it implies stated as something checkable.
#[derive(Debug, Clone)]
pub struct HumanDecision {
    pub text: String,
    /// A configuration and the exit code and output fragment `courier config check` must give.
    pub config_check: Option<(String, i32, String)>,
}

fn check(id: &'static str, description: &'static str, passed: bool, detail: String) -> Check {
    Check {
        id,
        description,
        passed,
        detail,
    }
}

fn describe(r: &Ran) -> String {
    format!(
        "exit {:?}; stdout {:?}; stderr {:?}",
        r.code,
        r.stdout.lines().next().unwrap_or(""),
        r.stderr.lines().next().unwrap_or("")
    )
}

const DEFAULT_PROBES: [&[&str]; 6] = [
    &["send", "GET", "http://h/a"],
    &["send", "GET", "http://h/x?fail=2"],
    &["send", "GET", "http://h/y?fail=3"],
    &["send", "GET", "http://h/status/404"],
    &["send", "GET", "http://h/z?fail=1&retry_after=90"],
    &["config", "check"],
];

/// Every check, run against the project in `dir`. `baseline_bin` is the original program, for
/// comparing default behaviour.
pub fn run(
    dir: &Path,
    baseline_bin: &Path,
    baseline_tests: usize,
    decision: Option<&HumanDecision>,
) -> Vec<Check> {
    let mut out = Vec::new();
    let Some(bin) = reality::build_binary(dir) else {
        return vec![check(
            "builds",
            "the project builds",
            false,
            "cargo build failed".into(),
        )];
    };
    out.push(check("builds", "the project builds", true, String::new()));

    // Defaults are preserved: the same probes give the same answers as the original program.
    let mut differing = Vec::new();
    for args in DEFAULT_PROBES {
        let (a, b) = (
            reality::run_binary(baseline_bin, args),
            reality::run_binary(&bin, args),
        );
        if a != b {
            differing.push(format!(
                "{args:?}: before {} / after {}",
                describe(&a),
                describe(&b)
            ));
        }
    }
    out.push(check(
        "defaults_preserved",
        "default behaviour is unchanged for the same requests",
        differing.is_empty(),
        differing.join("; "),
    ));

    // The retry budget is honoured when the server keeps sending Retry-After.
    let r = reality::run_binary(&bin, &["send", "GET", "http://h/w?fail=9&retry_after=1"]);
    out.push(check(
        "retry_after_spends_budget",
        "a server that keeps sending Retry-After cannot extend the attempt budget",
        r.code == Some(1)
            && r.stderr.contains("retries_exhausted")
            && r.stderr.contains("after 3 attempts"),
        describe(&r),
    ));

    // Configurable through the public configuration path.
    let cfg = reality::probe_config("retry5", "[retry]\nmax_attempts = 5\n");
    let c = cfg.to_str().unwrap();
    let r = reality::run_binary(&bin, &["--config", c, "send", "GET", "http://h/x?fail=4"]);
    out.push(check(
        "retry_configurable",
        "[retry] max_attempts reaches the executor",
        r.code == Some(0) && r.stdout.starts_with("200 OK"),
        describe(&r),
    ));

    // A route that does not set retry inherits the client's policy.
    let cfg = reality::probe_config(
        "route",
        "[retry]\nmax_attempts = 5\n[route.r]\nprefix = /r\ntimeout = 3s\n",
    );
    let r = reality::run_binary(
        &bin,
        &[
            "--config",
            cfg.to_str().unwrap(),
            "send",
            "GET",
            "http://h/r/x?fail=4",
        ],
    );
    out.push(check(
        "route_inherits_retry",
        "a route without its own retry settings uses the client's policy",
        r.code == Some(0) && r.stdout.starts_with("200 OK"),
        describe(&r),
    ));

    // Invalid configuration is rejected.
    let cfg = reality::probe_config("bad", "[retry]\nmax_attempts = 0\n");
    let r = reality::run_binary(
        &bin,
        &["--config", cfg.to_str().unwrap(), "config", "check"],
    );
    out.push(check(
        "invalid_rejected",
        "invalid retry configuration is rejected",
        r.code == Some(3),
        describe(&r),
    ));

    // Documentation.
    let docs = std::fs::read_to_string(dir.join("docs/configuration.md")).unwrap_or_default();
    out.push(check(
        "docs_updated",
        "docs/configuration.md documents [retry]",
        docs.lines().any(|l| l == "## [retry]") && docs.contains("`max_attempts`"),
        String::new(),
    ));

    // Coverage was added, not just code.
    let tests = reality::cargo_tests(dir);
    out.push(check(
        "coverage_added",
        "at least eight tests were added",
        tests.total() >= baseline_tests + 8,
        format!("{} tests before, {} after", baseline_tests, tests.total()),
    ));

    if let Some((config, code, fragment)) = decision.and_then(|d| d.config_check.as_ref()) {
        let cfg = reality::probe_config("decision", config);
        let r = reality::run_binary(
            &bin,
            &["--config", cfg.to_str().unwrap(), "config", "check"],
        );
        out.push(check(
            "human_decision_applied",
            "the behaviour the human decided is what the program does",
            r.code == Some(*code)
                && format!("{}{}", r.stdout, r.stderr).contains(fragment.as_str()),
            describe(&r),
        ));
    }
    out
}

pub fn unmet(checks: &[Check]) -> Vec<String> {
    checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| format!("{}: {}", c.id, c.description))
        .collect()
}
