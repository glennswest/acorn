//! Protocol error type.

use core::fmt;

/// Errors produced when parsing Seed wire formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// Datagram shorter than the fixed packet length.
    ShortPacket { got: usize, want: usize },
    /// Magic number did not match the expected packet type.
    BadMagic { got: u32, want: u32 },
    /// RVF file header failed validation.
    BadRvfHeader,
    /// Input ended mid-record.
    Truncated,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::ShortPacket { got, want } => {
                write!(f, "short packet: got {got} bytes, want {want}")
            }
            ProtoError::BadMagic { got, want } => {
                write!(f, "bad magic: got {got:#010x}, want {want:#010x}")
            }
            ProtoError::BadRvfHeader => write!(f, "bad RVF header"),
            ProtoError::Truncated => write!(f, "truncated input"),
        }
    }
}

impl std::error::Error for ProtoError {}
