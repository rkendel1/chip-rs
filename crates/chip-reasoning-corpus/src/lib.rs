//! A deterministic corpus of operational decisions for Chip's local reasoning
//! boundary, and a harness that replays it through any `LocalReasoner`.
//!
//! The corpus is the contract for what a local reasoner is responsible for:
//! it is structured data (never prompts), every case is decidable from its
//! input alone, and every expected verdict is explicit. The harness only reads
//! verdicts. A verdict is advice: nothing here executes, calls a model, or writes evidence.

mod corpus;
mod evaluate;

pub use corpus::{Category, Classification, ReasoningCase, Verdict, corpus};
pub use evaluate::{CaseResult, Disagreement, EvaluationResult, agreement, evaluate};
