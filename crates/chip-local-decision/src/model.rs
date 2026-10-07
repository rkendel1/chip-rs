//! The model: a linear classifier with two logits, and its compact binary artifact.
//!
//! `logit_k = bias_k + sum_j w_k[j] * x_j` over the dense features, the capability indicator and
//! the fact indicators. The decision is `Continue` only when the finite margin
//! `logit_continue - logit_escalate` exceeds the model's threshold; a tie, a NaN or an infinite
//! value is `Escalate`.
//!
//! Artifact layout (`chip.local-decision` v1, all numbers little-endian, floats IEEE-754 f32):
//! `"CHIPLDM\0"` | u16 version | u16 feature schema | u16 base features | u32 capabilities, each
//! `u16 len + utf-8` | u32 tokens, each `u16 len + name`, kind (1 bool, 2 int, 3 text) and value
//! (`u8` / `i64` / `u16 len + utf-8`) | f32 threshold | f32 bias[2] | f32 w_continue[D] |
//! f32 w_escalate[D], with `D = base + capabilities + tokens`, then a u32 CRC-32 (IEEE) of every
//! preceding byte. Parsing is exact: any deviation, including a bad checksum, trailing bytes or a
//! non-finite number, is an error.

use std::fmt;

use chip_core::CapabilityDecisionState;
use chip_wasm_decision::Decision;

use crate::features::{
    BASE_FEATURES, Features, MAX_TOKENS, Token, TokenValue, Vocabulary, extract,
};

pub const MODEL_SCHEMA: &str = "chip.local-decision.v1";
pub const ARTIFACT_VERSION: u16 = 1;
const FEATURE_SCHEMA_ID: u16 = 1;
const MAGIC: &[u8; 8] = b"CHIPLDM\0";
const MAX_CAPABILITIES: usize = 1024;
const MAX_VOCAB_TOKENS: usize = 4096;
const MAX_STRING: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    BadMagic,
    UnsupportedVersion(u16),
    UnsupportedFeatureSchema(u16),
    Truncated,
    TrailingBytes,
    BadChecksum,
    TooLarge,
    InvalidText,
    InvalidToken,
    NonFinite,
    /// Features that cannot have come from the extractor (out-of-range index, NaN, ...).
    InvalidFeatures,
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid local decision model: {self:?}")
    }
}

impl std::error::Error for ModelError {}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalDecisionModel {
    vocab: Vocabulary,
    weights: [Vec<f32>; 2],
    bias: [f32; 2],
    threshold: f32,
}

/// A raw learned answer and the margin it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Inference {
    pub decision: Decision,
    pub margin: f32,
}

impl LocalDecisionModel {
    /// Builds a model from parts. `weights[k]` covers dense, then capability, then token
    /// indicators.
    pub fn new(
        vocab: Vocabulary,
        weights: [Vec<f32>; 2],
        bias: [f32; 2],
        threshold: f32,
    ) -> Result<Self, ModelError> {
        let dimension = BASE_FEATURES + vocab.capabilities.len() + vocab.tokens.len();
        let finite = |xs: &[f32]| xs.iter().all(|x| x.is_finite());
        if weights[0].len() != dimension || weights[1].len() != dimension {
            return Err(ModelError::TrailingBytes);
        }
        if !finite(&weights[0]) || !finite(&weights[1]) || !finite(&bias) || !threshold.is_finite()
        {
            return Err(ModelError::NonFinite);
        }
        Ok(Self {
            vocab,
            weights,
            bias,
            threshold,
        })
    }

    pub fn vocabulary(&self) -> &Vocabulary {
        &self.vocab
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn dimension(&self) -> usize {
        self.weights[0].len()
    }

    /// Learned parameters: two weight vectors, two biases and the threshold.
    pub fn parameter_count(&self) -> usize {
        2 * self.dimension() + 3
    }

    pub fn weights(&self) -> (&[f32], &[f32]) {
        (&self.weights[0], &self.weights[1])
    }

    pub fn bias(&self) -> [f32; 2] {
        self.bias
    }

    /// Bytes the model occupies in memory (parameters plus vocabulary), not counting allocator
    /// overhead.
    pub fn resident_bytes(&self) -> usize {
        let strings: usize = self
            .vocab
            .capabilities
            .iter()
            .map(String::len)
            .sum::<usize>()
            + self
                .vocab
                .tokens
                .iter()
                .map(|t| {
                    t.name.len()
                        + match &t.value {
                            TokenValue::Text(s) => s.len(),
                            _ => 0,
                        }
                })
                .sum::<usize>();
        std::mem::size_of::<Self>()
            + self.parameter_count() * 4
            + self.vocab.capabilities.len() * std::mem::size_of::<String>()
            + self.vocab.tokens.len() * std::mem::size_of::<Token>()
            + strings
    }

    fn logit(&self, k: usize, f: &Features) -> f32 {
        let w = &self.weights[k];
        let mut z = self.bias[k];
        for (i, x) in f.base.iter().enumerate() {
            z += w[i] * x;
        }
        let capabilities = BASE_FEATURES + self.vocab.capabilities.len();
        if let Some(c) = f.capability {
            z += w[BASE_FEATURES + usize::from(c)];
        }
        for &t in &f.tokens[..f.token_count] {
            z += w[capabilities + usize::from(t)];
        }
        z
    }

    /// The two logits `[continue, escalate]`.
    pub fn logits(&self, f: &Features) -> [f32; 2] {
        [self.logit(0, f), self.logit(1, f)]
    }

    fn validate(&self, f: &Features) -> Result<(), ModelError> {
        if f.token_count > MAX_TOKENS
            || f.base.iter().any(|x| !x.is_finite())
            || f.capability
                .is_some_and(|c| usize::from(c) >= self.vocab.capabilities.len())
            || f.tokens[..f.token_count.min(MAX_TOKENS)]
                .iter()
                .any(|&t| usize::from(t) >= self.vocab.tokens.len())
        {
            return Err(ModelError::InvalidFeatures);
        }
        Ok(())
    }

    /// Decides from extracted features. Invalid features are an error, never a decision.
    pub fn infer_features(&self, f: &Features) -> Result<Inference, ModelError> {
        self.validate(f)?;
        let [c, e] = self.logits(f);
        let margin = c - e;
        let decision = if margin.is_finite() && margin > self.threshold {
            Decision::Continue
        } else {
            Decision::Escalate
        };
        Ok(Inference { decision, margin })
    }

    /// Features, model and mapping in one call. Allocation-free.
    pub fn infer(&self, state: &CapabilityDecisionState) -> Inference {
        // The extractor only emits indices into this vocabulary, so validation cannot fail.
        self.infer_features(&extract(&self.vocab, state))
            .unwrap_or(Inference {
                decision: Decision::Escalate,
                margin: f32::NEG_INFINITY,
            })
    }

    /// The compact, stable artifact.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.parameter_count() * 4);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&ARTIFACT_VERSION.to_le_bytes());
        out.extend_from_slice(&FEATURE_SCHEMA_ID.to_le_bytes());
        out.extend_from_slice(&(BASE_FEATURES as u16).to_le_bytes());
        let put_str = |out: &mut Vec<u8>, s: &str| {
            out.extend_from_slice(&(s.len() as u16).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        out.extend_from_slice(&(self.vocab.capabilities.len() as u32).to_le_bytes());
        for c in &self.vocab.capabilities {
            put_str(&mut out, c);
        }
        out.extend_from_slice(&(self.vocab.tokens.len() as u32).to_le_bytes());
        for t in &self.vocab.tokens {
            put_str(&mut out, &t.name);
            match &t.value {
                TokenValue::Bool(b) => out.extend_from_slice(&[1, u8::from(*b)]),
                TokenValue::Int(i) => {
                    out.push(2);
                    out.extend_from_slice(&i.to_le_bytes());
                }
                TokenValue::Text(s) => {
                    out.push(3);
                    put_str(&mut out, s);
                }
            }
        }
        out.extend_from_slice(&self.threshold.to_le_bytes());
        for b in self.bias {
            out.extend_from_slice(&b.to_le_bytes());
        }
        for k in 0..2 {
            for w in &self.weights[k] {
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
        let crc = crc32(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parses an artifact. Anything unexpected is an error; nothing is guessed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ModelError> {
        let mut r = Reader { rest: bytes };
        if r.take(8)? != MAGIC {
            return Err(ModelError::BadMagic);
        }
        let version = r.u16()?;
        if version != ARTIFACT_VERSION {
            return Err(ModelError::UnsupportedVersion(version));
        }
        let schema = r.u16()?;
        if schema != FEATURE_SCHEMA_ID || usize::from(r.u16()?) != BASE_FEATURES {
            return Err(ModelError::UnsupportedFeatureSchema(schema));
        }
        // Header understood: now the whole artifact must be intact before anything else is read.
        let Some(body_len) = bytes.len().checked_sub(4) else {
            return Err(ModelError::Truncated);
        };
        if crc32(&bytes[..body_len]) != u32::from_le_bytes(bytes[body_len..].try_into().unwrap()) {
            return Err(ModelError::BadChecksum);
        }
        r.rest = &bytes[bytes.len() - r.rest.len()..body_len];
        let n_caps = r.u32()? as usize;
        if n_caps > MAX_CAPABILITIES {
            return Err(ModelError::TooLarge);
        }
        let mut capabilities = Vec::with_capacity(n_caps);
        for _ in 0..n_caps {
            capabilities.push(r.string()?);
        }
        let n_tokens = r.u32()? as usize;
        if n_tokens > MAX_VOCAB_TOKENS {
            return Err(ModelError::TooLarge);
        }
        let mut tokens = Vec::with_capacity(n_tokens);
        for _ in 0..n_tokens {
            let name = r.string()?;
            let value = match r.u8()? {
                1 => match r.u8()? {
                    0 => TokenValue::Bool(false),
                    1 => TokenValue::Bool(true),
                    _ => return Err(ModelError::InvalidToken),
                },
                2 => TokenValue::Int(i64::from_le_bytes(r.take(8)?.try_into().unwrap())),
                3 => TokenValue::Text(r.string()?),
                _ => return Err(ModelError::InvalidToken),
            };
            tokens.push(Token { name, value });
        }
        let threshold = r.f32()?;
        let bias = [r.f32()?, r.f32()?];
        let dimension = BASE_FEATURES + n_caps + n_tokens;
        let mut weights = [Vec::with_capacity(dimension), Vec::with_capacity(dimension)];
        for w in &mut weights {
            for _ in 0..dimension {
                w.push(r.f32()?);
            }
        }
        if !r.rest.is_empty() {
            return Err(ModelError::TrailingBytes);
        }
        Self::new(
            Vocabulary {
                capabilities,
                tokens,
            },
            weights,
            bias,
            threshold,
        )
    }

    /// The model committed to this repository, embedded at compile time: no file is read at
    /// run time.
    pub fn embedded() -> Result<Self, ModelError> {
        Self::from_bytes(include_bytes!("../models/local-decision-v1.bin"))
    }
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ModelError> {
        if self.rest.len() < n {
            return Err(ModelError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, ModelError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ModelError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, ModelError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn f32(&mut self) -> Result<f32, ModelError> {
        let x = f32::from_le_bytes(self.take(4)?.try_into().unwrap());
        if x.is_finite() {
            Ok(x)
        } else {
            Err(ModelError::NonFinite)
        }
    }

    fn string(&mut self) -> Result<String, ModelError> {
        let len = usize::from(self.u16()?);
        if len > MAX_STRING {
            return Err(ModelError::TooLarge);
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| ModelError::InvalidText)
    }
}

/// CRC-32 (IEEE 802.3), bitwise: tiny, dependency-free, enough to catch any single-byte damage.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
