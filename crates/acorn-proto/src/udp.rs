//! UDP wire protocol: ESP32 -> Seed ingest path.
//!
//! Three packet types share the ingest path. `0xC5110003` (the 8-dim feature
//! packet) is the one a wire-compatible Seed ingests into the RVF store; the
//! definitions for the other two magics are kept here for completeness.
//!
//! All multi-byte fields are little-endian on the wire (ESP32-S3 is LE).

use crate::error::ProtoError;

/// Raw CSI frame magic (UDP :5005).
pub const RAW_CSI_MAGIC: u32 = 0xC511_0001;
/// Vitals frame magic (UDP :5006).
pub const VITALS_MAGIC: u32 = 0xC511_0002;
/// 8-dim feature packet magic (UDP :5006) — the packet we ingest.
pub const FEATURE_MAGIC: u32 = 0xC511_0003;

/// Wire size of [`EdgeFeaturePkt`]. Enforced by ADR-069 (`static_assert`).
pub const FEATURE_PKT_LEN: usize = 48;

/// `edge_feature_pkt_t` from ADR-069 — a 48-byte packed, little-endian frame.
///
/// `features` is the 8-dim normalized vector; see [`crate::event::FeatureVector`]
/// for the dimension semantics and de-normalization back to physical units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeFeaturePkt {
    pub magic: u32,
    pub node_id: u8,
    pub reserved: u8,
    pub seq: u16,
    pub timestamp_us: i64,
    pub features: [f32; 8],
}

impl EdgeFeaturePkt {
    /// Parse from a little-endian datagram. Validates length and magic.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < FEATURE_PKT_LEN {
            return Err(ProtoError::ShortPacket {
                got: buf.len(),
                want: FEATURE_PKT_LEN,
            });
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != FEATURE_MAGIC {
            return Err(ProtoError::BadMagic {
                got: magic,
                want: FEATURE_MAGIC,
            });
        }
        let seq = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        let timestamp_us = i64::from_le_bytes(buf[8..16].try_into().unwrap());
        let mut features = [0.0f32; 8];
        for (i, slot) in features.iter_mut().enumerate() {
            let off = 16 + i * 4;
            *slot = f32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        }
        Ok(Self {
            magic,
            node_id: buf[4],
            reserved: buf[5],
            seq,
            timestamp_us,
            features,
        })
    }

    /// Serialize to a 48-byte little-endian datagram.
    pub fn to_bytes(&self) -> [u8; FEATURE_PKT_LEN] {
        let mut b = [0u8; FEATURE_PKT_LEN];
        b[0..4].copy_from_slice(&self.magic.to_le_bytes());
        b[4] = self.node_id;
        b[5] = self.reserved;
        b[6..8].copy_from_slice(&self.seq.to_le_bytes());
        b[8..16].copy_from_slice(&self.timestamp_us.to_le_bytes());
        for (i, f) in self.features.iter().enumerate() {
            let off = 16 + i * 4;
            b[off..off + 4].copy_from_slice(&f.to_le_bytes());
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_pkt_roundtrips() {
        let pkt = EdgeFeaturePkt {
            magic: FEATURE_MAGIC,
            node_id: 1,
            reserved: 0,
            seq: 42,
            timestamp_us: 1_775_166_970_000_000,
            features: [0.85, 0.3, 0.52, 0.65, 0.4, 0.78, 0.0, 0.55],
        };
        let bytes = pkt.to_bytes();
        assert_eq!(bytes.len(), FEATURE_PKT_LEN);
        assert_eq!(EdgeFeaturePkt::from_bytes(&bytes).unwrap(), pkt);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = [0u8; FEATURE_PKT_LEN];
        bytes[0..4].copy_from_slice(&RAW_CSI_MAGIC.to_le_bytes());
        assert!(matches!(
            EdgeFeaturePkt::from_bytes(&bytes),
            Err(ProtoError::BadMagic { .. })
        ));
    }
}
