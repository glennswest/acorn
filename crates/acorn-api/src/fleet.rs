//! Per-node fleet observability.
//!
//! Tracks every ESP32 source that has sent a packet, with last-seen time,
//! packet count, last sequence number, and detected gaps (received seq
//! skipped expected next, modulo 2^16 wrap).

use std::collections::BTreeMap;

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use parking_lot::RwLock;
use serde::Serialize;

use crate::{require_bearer, ApiError, AppState};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct NodeState {
    pub node_id: u8,
    /// Pi wall-clock μs since UNIX epoch at the moment of last packet receive.
    /// THIS is the meaningful "last seen" for an operator looking at the
    /// fleet — independent of whatever clock the ESP32 happens to be using.
    pub last_received_us: i64,
    /// ESP32's reported timestamp_us from the packet. Often relative to
    /// node boot (if no NTP), so don't display as wall-clock without
    /// knowing the source.
    pub last_node_clock_us: i64,
    pub packet_count: u64,
    pub last_seq: u16,
    pub gaps: u64,
    pub last_features: [f32; 8],
}

#[derive(Default)]
pub struct NodeRegistry {
    inner: RwLock<BTreeMap<u8, NodeState>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a packet. Updates last_received_us (Pi-side wall clock),
    /// last_node_clock_us (the ESP32-supplied μs), seq/count, and detects
    /// skips.
    pub fn observe(&self, node_id: u8, seq: u16, node_ts_us: i64, features: [f32; 8]) {
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let mut m = self.inner.write();
        let entry = m.entry(node_id).or_insert(NodeState {
            node_id,
            last_received_us: now_us,
            last_node_clock_us: node_ts_us,
            packet_count: 0,
            last_seq: seq.wrapping_sub(1),
            gaps: 0,
            last_features: features,
        });
        // Expected next = last_seq + 1 (mod 2^16). Anything else implies a
        // skip; we count the size of the skip (excluding ourselves).
        let expected = entry.last_seq.wrapping_add(1);
        if entry.packet_count > 0 && seq != expected {
            let skipped = seq.wrapping_sub(expected) as u64;
            entry.gaps = entry.gaps.saturating_add(skipped);
        }
        entry.last_received_us = now_us;
        entry.last_node_clock_us = node_ts_us;
        entry.packet_count = entry.packet_count.saturating_add(1);
        entry.last_seq = seq;
        entry.last_features = features;
    }

    pub fn list(&self) -> Vec<NodeState> {
        self.inner.read().values().copied().collect()
    }

    pub fn get(&self, id: u8) -> Option<NodeState> {
        self.inner.read().get(&id).copied()
    }
}

pub async fn handle_nodes_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<NodeState>>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    Ok(Json(state.nodes.list()))
}

pub async fn handle_node_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u8>,
) -> Result<Json<NodeState>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    state
        .nodes
        .get(id)
        .map(Json)
        .ok_or(ApiError::BadRequest(format!("no such node: {id}")))
}
