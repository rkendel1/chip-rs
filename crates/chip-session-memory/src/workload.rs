//! A deterministic session workload, used by the tests and the benchmark.
//!
//! Events are produced lazily and statelessly from `(seed, index)`, so the generator holds no
//! payloads and the same seed always yields the same bytes. Every event refers only to things an
//! earlier event created, so a session built from the stream is internally consistent.
//!
//! Shape, for `observations = N`:
//!
//! * N observations. Sizes: 60% 1 KiB, 30% 8 KiB, 9% 32 KiB, 1% 128 KiB. About 30% repeat an
//!   earlier observation's payload exactly (repeated diagnostics), which supersedes the earlier one.
//! * every 25th observation is a test run, cycling fail, fail, fail, pass for the command `unit`;
//! * every 100th: two failed attempts, an open hypothesis, and, at the pass, a verified repair;
//! * every 1000th: a plan revision and a checkpoint; every 2500th: an escalation;
//! * at the end: a failing `integration` test that stays unresolved, an open hypothesis citing its
//!   diagnostic, a pending decision and a final escalation and checkpoint.

use crate::schema::{Outcome, PendingAction, Reference};

#[derive(Debug, Clone)]
pub enum Event {
    Task {
        id: String,
        goal: String,
    },
    Plan {
        steps: Vec<String>,
    },
    Observation {
        id: String,
        kind: String,
        provenance: String,
        summary: String,
        payload: String,
        /// The earlier observation whose request this one repeats, if any.
        repeats: Option<String>,
    },
    Supersede {
        old: String,
        by: String,
    },
    Attempt {
        id: String,
        action: String,
        outcome: Outcome,
        refs: Vec<Reference>,
    },
    TestResult {
        id: String,
        command: String,
        passed: bool,
        failed_tests: Vec<String>,
        diagnostic: String,
    },
    Repair {
        id: String,
        fix: String,
        outcome: Outcome,
        verified_by: Option<String>,
    },
    Hypothesis {
        id: String,
        explanation: String,
        evidence: Vec<Reference>,
    },
    ResolveHypothesis {
        id: String,
    },
    Escalation {
        id: String,
        reason: String,
        prior_attempts: Vec<String>,
        known_failures: Vec<String>,
        successes: Vec<String>,
        outstanding: Vec<String>,
    },
    Pending(PendingAction),
    Checkpoint {
        id: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Workload {
    pub observations: usize,
    pub seed: u64,
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn id(i: usize) -> String {
    format!("o{i:05}")
}

impl Workload {
    pub fn new(observations: usize, seed: u64) -> Self {
        Self { observations, seed }
    }

    pub fn objective(&self) -> String {
        "Make the integration tests pass without weakening any test, and escalate with full context if that cannot be established.".into()
    }

    fn r(&self, i: usize, salt: u64) -> u64 {
        mix(self.seed ^ mix(i as u64 ^ salt.wrapping_mul(0x9e3779b97f4a7c15)))
    }

    /// The payload size for observation `i`, in bytes.
    pub fn payload_size(&self, i: usize) -> usize {
        match self.r(i, 1) % 100 {
            0 => 128 * 1024,
            1..=9 => 32 * 1024,
            10..=39 => 8 * 1024,
            _ => 1024,
        }
    }

    /// The payload of observation `i`: diagnostic-looking text of exactly `payload_size(i)` bytes.
    pub fn payload_for(&self, i: usize) -> String {
        let target = self.payload_size(i);
        let mut out = String::with_capacity(target + 128);
        let mut n = 0u64;
        while out.len() < target {
            let h = self.r(i, 100 + n);
            out.push_str(&format!(
                "error[E{:04}]: mismatched types in module m{} (obs {i}, line {n})\n  --> src/m{}.rs:{}:{}\n",
                h % 9999, h % 53, h % 53, h % 400, h % 80
            ));
            n += 1;
        }
        out.truncate(target);
        out
    }

    fn kind(&self, i: usize) -> &'static str {
        if i % 25 == 24 {
            return "test";
        }
        ["list", "search", "read", "read", "read", "git"][(self.r(i, 2) % 6) as usize]
    }

    /// The earlier observation that observation `i` repeats, if it does.
    pub fn duplicate_of(&self, i: usize) -> Option<usize> {
        if i >= 30 && i % 25 != 24 && self.r(i, 3) % 100 < 30 {
            Some(i - 1 - (self.r(i, 4) % 29) as usize)
        } else {
            None
        }
    }

    /// Every event, in order.
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        let head = std::iter::once(Event::Task {
            id: "t1".into(),
            goal: "make the integration tests pass".into(),
        });
        let body = (0..self.observations).flat_map(move |i| self.events_for(i));
        let tail = self.tail().into_iter();
        head.chain(body).chain(tail)
    }

    fn events_for(&self, i: usize) -> Vec<Event> {
        let mut out = Vec::new();
        if i % 1000 == 0 {
            out.push(Event::Plan {
                steps: vec![
                    format!("revision {}: inspect, fix, retest", i / 1000 + 1),
                    "never weaken a test".into(),
                ],
            });
        }
        let dup = self.duplicate_of(i);
        let payload = self.payload_for(dup.unwrap_or(i));
        let kind = self.kind(i);
        out.push(Event::Observation {
            id: id(i),
            kind: kind.into(),
            provenance: format!("exec:{:x}-{i}", self.seed),
            summary: format!("{kind} #{i}: {} bytes", payload.len()),
            payload,
            repeats: dup.map(id),
        });
        if let Some(j) = dup {
            out.push(Event::Supersede {
                old: id(j),
                by: id(i),
            });
        }
        if i % 25 == 24 {
            let pass = (i / 25) % 4 == 3;
            out.push(Event::TestResult {
                id: format!("t{i}"),
                command: "unit".into(),
                passed: pass,
                failed_tests: if pass {
                    vec![]
                } else {
                    vec![format!("unit::case_{}", i / 25)]
                },
                diagnostic: id(i),
            });
        }
        if i % 100 == 49 {
            for k in 0..2 {
                out.push(Event::Attempt {
                    id: format!("a{i}-{k}"),
                    action: format!("edit src/m{}.rs (variant {k})", (i / 100) % 53),
                    outcome: Outcome::Failed,
                    refs: vec![Reference {
                        kind: "obs".into(),
                        id: id(i),
                    }],
                });
            }
            out.push(Event::Hypothesis {
                id: format!("h{i}"),
                explanation: format!(
                    "the failure near obs {i} comes from module m{}",
                    (i / 100) % 53
                ),
                evidence: vec![Reference {
                    kind: "obs".into(),
                    id: id(i),
                }],
            });
        }
        if i % 100 == 99 {
            out.push(Event::Attempt {
                id: format!("a{i}"),
                action: format!("fix src/m{}.rs", (i / 100) % 53),
                outcome: Outcome::Succeeded,
                refs: vec![Reference {
                    kind: "obs".into(),
                    id: id(i),
                }],
            });
            out.push(Event::Repair {
                id: format!("r{i}"),
                fix: format!("fix src/m{}.rs", (i / 100) % 53),
                outcome: Outcome::Succeeded,
                verified_by: Some(format!("t{i}")),
            });
            out.push(Event::ResolveHypothesis {
                id: format!("h{}", i - 50),
            });
        }
        if i % 2500 == 2499 {
            out.push(Event::Escalation {
                id: format!("e{i}"),
                reason: "the same integration test keeps failing after repeated repairs".into(),
                prior_attempts: vec![format!("a{}-0", i - 50), format!("a{}", i)],
                known_failures: vec![format!("edit variant 0 near obs {}", i - 50)],
                successes: vec![format!("repair r{i} verified by t{i}")],
                outstanding: vec![
                    "is the integration expectation or the implementation authoritative?".into(),
                ],
            });
        }
        if i % 1000 == 999 {
            out.push(Event::Checkpoint {
                id: format!("c{i}"),
            });
        }
        out
    }

    fn tail(&self) -> Vec<Event> {
        let n = self.observations;
        let diag = "final-diagnostic".to_string();
        let mut payload = String::new();
        let mut k = 0u64;
        while payload.len() < 16 * 1024 {
            payload.push_str(&format!("thread 'integration::route_inherits' panicked at tests/retry.rs:{k}\n  left: 3\n right: 5\n"));
            k += 1;
        }
        vec![
            Event::Observation {
                id: diag.clone(),
                kind: "test".into(),
                provenance: format!("exec:{:x}-final", self.seed),
                summary: "integration run failed: route_inherits".into(),
                payload,
                repeats: None,
            },
            Event::TestResult {
                id: "t-final".into(),
                command: "integration".into(),
                passed: false,
                failed_tests: vec!["integration::route_inherits".into()],
                diagnostic: diag.clone(),
            },
            Event::Hypothesis {
                id: "h-final".into(),
                explanation:
                    "route settings drop the new retry policy when building route overrides".into(),
                evidence: vec![Reference {
                    kind: "obs".into(),
                    id: diag,
                }],
            },
            Event::Pending(PendingAction {
                description:
                    "decide whether route-level retry replaces or merges with the client policy"
                        .into(),
                decision_needed: true,
            }),
            Event::Escalation {
                id: "e-final".into(),
                reason: "two plausible explanations and no evidence that distinguishes them".into(),
                prior_attempts: vec![format!("a{}", ((n.max(100) - 1) / 100).max(1) * 100 - 1)],
                known_failures: vec!["changing the executor did not change the failure".into()],
                successes: vec![],
                outstanding: vec!["replace or merge?".into()],
            },
            Event::Checkpoint {
                id: "c-final".into(),
            },
        ]
    }
}
