//! RVF (RuVector Format) — the append-only vector-store file format.
//!
//! # ⚠ Provisional layout
//!
//! The byte layout below is **reconstructed** from ADR-069's storage-budget
//! arithmetic (~40 bytes/record for 8×f32 vectors plus a content-addressed
//! ID), *not* from a published specification. Before relying on it for
//! interop, confirm against one of:
//!   1. `hexdump -C` of a real `GET /api/v1/store/export`, or
//!   2. the `wifi-densepose-train` crate that parses `.rvf` files.
//!
//! See `cognitum-seed-rust-scoping.md` §0 and §2. Treat [`RvfRecord::WIRE_LEN`]
//! as the single tunable once the real stride is known.

use crate::error::ProtoError;

/// File magic. Confirm the real 4 bytes against an export.
pub const RVF_MAGIC: [u8; 4] = *b"RVF1";
/// Vector dimensionality for the CSI feature store.
pub const RVF_DIM: u16 = 8;
/// On-disk size of [`RvfHeader`]. 18 bytes of data + 14 bytes reserved = 32.
pub const RVF_HEADER_LEN: usize = 32;
/// Current header version emitted by this implementation.
pub const RVF_VERSION: u16 = 1;

/// Distance metric tag stored in the RVF header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cosine = 0,
    L2 = 1,
    Dot = 2,
}

impl Metric {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            0 => Ok(Self::Cosine),
            1 => Ok(Self::L2),
            2 => Ok(Self::Dot),
            _ => Err(ProtoError::BadRvfHeader),
        }
    }
}

/// RVF file header (written once, provisional field widths).
///
/// Wire layout (32 bytes total, all little-endian):
/// `0..4 magic("RVF1") | 4..6 version u16 | 6..8 dim u16 | 8 metric u8 |
///  9 flags u8 | 10..18 created_us i64 | 18..32 reserved (zeros)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RvfHeader {
    pub version: u16,
    pub dim: u16,
    pub metric: u8,
    pub flags: u8,
    pub created_us: i64,
}

impl RvfHeader {
    pub const WIRE_LEN: usize = RVF_HEADER_LEN;

    /// Build a fresh header with `created_us` set to the current wall clock.
    pub fn current(metric: Metric, dim: u16) -> Self {
        let created_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        Self {
            version: RVF_VERSION,
            dim,
            metric: metric as u8,
            flags: 0,
            created_us,
        }
    }

    pub fn to_bytes(&self) -> [u8; RVF_HEADER_LEN] {
        let mut b = [0u8; RVF_HEADER_LEN];
        b[0..4].copy_from_slice(&RVF_MAGIC);
        b[4..6].copy_from_slice(&self.version.to_le_bytes());
        b[6..8].copy_from_slice(&self.dim.to_le_bytes());
        b[8] = self.metric;
        b[9] = self.flags;
        b[10..18].copy_from_slice(&self.created_us.to_le_bytes());
        b
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < RVF_HEADER_LEN {
            return Err(ProtoError::Truncated);
        }
        if buf[0..4] != RVF_MAGIC {
            return Err(ProtoError::BadRvfHeader);
        }
        Ok(Self {
            version: u16::from_le_bytes(buf[4..6].try_into().unwrap()),
            dim: u16::from_le_bytes(buf[6..8].try_into().unwrap()),
            metric: buf[8],
            flags: buf[9],
            created_us: i64::from_le_bytes(buf[10..18].try_into().unwrap()),
        })
    }
}

/// One append-only RVF record. **Provisional ~42-byte layout.**
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RvfRecord {
    /// Content-addressed ID — truncated SHA-256 of `node_id:ts_us:seq`.
    pub id: u32,
    pub vector: [f32; 8],
    pub node_id: u8,
    pub type_tag: u8,
    pub timestamp: u32,
}

impl RvfRecord {
    /// Provisional on-disk stride. Reconcile against a real export.
    pub const WIRE_LEN: usize = 4 + 32 + 1 + 1 + 4;

    /// Serialize one record (little-endian).
    pub fn to_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut b = [0u8; Self::WIRE_LEN];
        b[0..4].copy_from_slice(&self.id.to_le_bytes());
        for (i, f) in self.vector.iter().enumerate() {
            b[4 + i * 4..8 + i * 4].copy_from_slice(&f.to_le_bytes());
        }
        b[36] = self.node_id;
        b[37] = self.type_tag;
        b[38..42].copy_from_slice(&self.timestamp.to_le_bytes());
        b
    }

    /// Parse one record from the head of `buf`.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::WIRE_LEN {
            return Err(ProtoError::Truncated);
        }
        let id = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let mut vector = [0.0f32; 8];
        for (i, slot) in vector.iter_mut().enumerate() {
            *slot = f32::from_le_bytes(buf[4 + i * 4..8 + i * 4].try_into().unwrap());
        }
        Ok(Self {
            id,
            vector,
            node_id: buf[36],
            type_tag: buf[37],
            timestamp: u32::from_le_bytes(buf[38..42].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrips() {
        let r = RvfRecord {
            id: 0xDEAD_BEEF,
            vector: [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8],
            node_id: 3,
            type_tag: 1,
            timestamp: 1_775_166_970,
        };
        assert_eq!(RvfRecord::from_bytes(&r.to_bytes()).unwrap(), r);
    }

    #[test]
    fn header_roundtrips() {
        let h = RvfHeader {
            version: RVF_VERSION,
            dim: 8,
            metric: Metric::Cosine as u8,
            flags: 0,
            created_us: 1_775_166_970_000_000,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes[0..4], RVF_MAGIC);
        assert_eq!(RvfHeader::from_bytes(&bytes).unwrap(), h);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut bytes = [0u8; RVF_HEADER_LEN];
        bytes[0..4].copy_from_slice(b"XXXX");
        assert!(matches!(
            RvfHeader::from_bytes(&bytes),
            Err(ProtoError::BadRvfHeader)
        ));
    }
}
