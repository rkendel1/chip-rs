use std::fmt;

/// A 128-bit identifier written as 32 lowercase hexadecimal characters (log generations, event
/// ids, authority ids).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Identifier(pub [u8; 16]);

impl Identifier {
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 32 {
            return None;
        }
        let mut out = [0u8; 16];
        for (index, pair) in bytes.chunks(2).enumerate() {
            out[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(out))
    }
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identifier({self})")
    }
}

/// Where new identifiers (log generations, event ids, authority ids, operation ids) come from.
pub trait IdSource {
    fn next_id(&mut self) -> std::io::Result<Identifier>;
}

/// Identifiers from the operating system's random source.
pub struct OsIds;

impl IdSource for OsIds {
    fn next_id(&mut self) -> std::io::Result<Identifier> {
        use std::io::Read;
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(Identifier(bytes))
    }
}
