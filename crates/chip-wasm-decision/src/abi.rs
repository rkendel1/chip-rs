//! The wire contract: how canonical state bytes are read, and what the result codes mean.
//!
//! The input is exactly `CapabilityDecisionState::canonical_bytes()` from `chip-core`
//! (all integers big-endian, every variable-length field length-prefixed):
//!
//! `"chip.decision.v1"` 0x00 | u32 len, capability | 32-byte graph state | evidence code
//! (1 valid, 2 stale, 3 unknown) | impact code (1 impacted, 2 unchanged) | u32 input count |
//! per input: u32 len, name, type code (1 text: u32 len + bytes; 2 integer: i64; 3 bool: 0|1)
//!
//! Parsing is strict and exact: any deviation, including trailing bytes, is invalid.

/// The ABI version, exported by the module and compared byte for byte by the host.
pub const ABI_VERSION: &str = "chip.decision.v1";

pub const CONTINUE: u8 = 0;
pub const ESCALATE: u8 = 1;
/// Reserved. Never a decision; the host maps it to an error.
pub const INVALID: u8 = 255;

/// Largest state the module accepts. Far above a realistic state (64 bytes minimal).
pub const MAX_STATE_BYTES: usize = 4096;

const MAX_CAPABILITY_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbiError {
    TooLong,
    Truncated,
    TrailingBytes,
    UnknownVersion,
    InvalidCapability,
    InvalidEvidence,
    InvalidImpact,
    InvalidInput,
}

/// What the decision needs, borrowed from the input. Nothing is copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedState {
    pub evidence: Evidence,
    pub impact: Impact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    Valid,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Impact {
    Impacted,
    Unchanged,
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], AbiError> {
        if self.rest.len() < n {
            return Err(AbiError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn byte(&mut self) -> Result<u8, AbiError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<usize, AbiError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    fn text(&mut self) -> Result<&'a str, AbiError> {
        let len = self.u32()?;
        core::str::from_utf8(self.take(len)?).map_err(|_| AbiError::InvalidInput)
    }
}

fn valid_capability(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CAPABILITY_BYTES
        && id.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

/// Parses and validates canonical state bytes.
pub fn parse(bytes: &[u8]) -> Result<ParsedState, AbiError> {
    if bytes.len() > MAX_STATE_BYTES {
        return Err(AbiError::TooLong);
    }
    let mut r = Reader { rest: bytes };
    let version = r.take(ABI_VERSION.len())?;
    if version != ABI_VERSION.as_bytes() || r.byte()? != 0 {
        return Err(AbiError::UnknownVersion);
    }
    let capability = r.text().map_err(|e| match e {
        AbiError::InvalidInput => AbiError::InvalidCapability,
        other => other,
    })?;
    if !valid_capability(capability) {
        return Err(AbiError::InvalidCapability);
    }
    r.take(32)?; // graph state: any 32 bytes, but exactly 32
    let evidence = match r.byte()? {
        1 => Evidence::Valid,
        2 => Evidence::Stale,
        3 => Evidence::Unknown,
        _ => return Err(AbiError::InvalidEvidence),
    };
    let impact = match r.byte()? {
        1 => Impact::Impacted,
        2 => Impact::Unchanged,
        _ => return Err(AbiError::InvalidImpact),
    };
    let count = r.u32()?;
    for _ in 0..count {
        r.text()?;
        match r.byte()? {
            1 => {
                r.text()?;
            }
            2 => {
                r.take(8)?;
            }
            3 => {
                if r.byte()? > 1 {
                    return Err(AbiError::InvalidInput);
                }
            }
            _ => return Err(AbiError::InvalidInput),
        }
    }
    if !r.rest.is_empty() {
        return Err(AbiError::TrailingBytes);
    }
    Ok(ParsedState { evidence, impact })
}
