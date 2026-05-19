//! UDP ingest task for `acornd`.
//!
//! Listens for [`EdgeFeaturePkt`] datagrams on the configured port, converts
//! each to an [`RvfRecord`] with a content-addressed ID, then appends to the
//! witness chain and the store. Bad/short packets are dropped silently
//! (logged at debug level).

use std::{net::SocketAddr, sync::Arc};

use acorn_proto::rvf::RvfRecord;
use acorn_proto::udp::{EdgeFeaturePkt, FEATURE_PKT_LEN};
use acorn_store::RvfStore;
use acorn_witness::WitnessChain;
use sha2::{Digest, Sha256};
use tokio::net::UdpSocket;

pub async fn run(
    addr: SocketAddr,
    store: Arc<RvfStore>,
    witness: Arc<WitnessChain>,
) -> anyhow::Result<()> {
    let sock = UdpSocket::bind(addr).await?;
    tracing::info!(%addr, "udp ingest listening");
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, from) = sock.recv_from(&mut buf).await?;
        if n < FEATURE_PKT_LEN {
            tracing::debug!(n, %from, "short packet dropped");
            continue;
        }
        let pkt = match EdgeFeaturePkt::from_bytes(&buf[..n]) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(?e, %from, "malformed packet dropped");
                continue;
            }
        };
        let rec = packet_to_record(&pkt);
        if let Err(e) = witness.append(&rec.to_bytes()) {
            tracing::warn!(?e, "witness append failed; dropping record");
            continue;
        }
        if let Err(e) = store.append_batch(&[rec]) {
            tracing::warn!(
                ?e,
                "store append failed AFTER witness commit — chain ahead of store"
            );
        }
    }
}

/// Content-addressed ID: truncated SHA-256 of `node_id || ts_us || seq`.
pub fn packet_to_record(pkt: &EdgeFeaturePkt) -> RvfRecord {
    let mut h = Sha256::new();
    h.update(pkt.node_id.to_le_bytes());
    h.update(pkt.timestamp_us.to_le_bytes());
    h.update(pkt.seq.to_le_bytes());
    let d = h.finalize();
    let id = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
    RvfRecord {
        id,
        vector: pkt.features,
        node_id: pkt.node_id,
        type_tag: 1, // csi_feature
        timestamp: (pkt.timestamp_us / 1_000_000) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acorn_proto::udp::FEATURE_MAGIC;

    #[test]
    fn id_is_deterministic_for_same_packet() {
        let pkt = EdgeFeaturePkt {
            magic: FEATURE_MAGIC,
            node_id: 7,
            reserved: 0,
            seq: 1234,
            timestamp_us: 1_775_000_000_000_000,
            features: [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8],
        };
        let a = packet_to_record(&pkt);
        let b = packet_to_record(&pkt);
        assert_eq!(a.id, b.id);
        assert_eq!(a.vector, pkt.features);
        assert_eq!(a.node_id, 7);
        assert_eq!(a.type_tag, 1);
        assert_eq!(a.timestamp, 1_775_000_000);
    }

    #[test]
    fn different_seq_yields_different_id() {
        let mut pkt = EdgeFeaturePkt {
            magic: FEATURE_MAGIC,
            node_id: 1,
            reserved: 0,
            seq: 0,
            timestamp_us: 1_000_000,
            features: [0.0; 8],
        };
        let a = packet_to_record(&pkt).id;
        pkt.seq = 1;
        let b = packet_to_record(&pkt).id;
        assert_ne!(a, b);
    }
}
