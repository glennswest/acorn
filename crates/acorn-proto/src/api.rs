//! HTTP API request/response DTOs — the RuView-facing JSON surface.
//!
//! Shapes mirror the curl examples in ADR-069. Field names are chosen to
//! serialize to the exact JSON RuView's bridge sends/expects.

use serde::{Deserialize, Serialize};

/// `POST /api/v1/pair` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub client_name: String,
}

/// `POST /api/v1/pair` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    pub token: String,
    pub device_id: String,
}

/// One `(id, vector)` pair. Serializes as `[0, [..8 floats..]]` per ADR-069.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorEntry(pub u32, pub [f32; 8]);

/// Metadata block attached to an ingest batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestMetadata {
    pub node_id: u8,
    /// JSON key is `type` (e.g. `"csi_feature"`).
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: i64,
}

/// `POST /api/v1/store/ingest` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestRequest {
    pub vectors: Vec<VectorEntry>,
    pub metadata: IngestMetadata,
}

/// `POST /api/v1/store/ingest` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResponse {
    pub accepted: usize,
    pub epoch: u64,
    pub witness_head: String,
}

/// `POST /api/v1/store/query` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub vector: [f32; 8],
    pub k: usize,
}

/// A single kNN hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryHit {
    pub id: u32,
    pub distance: f32,
}

/// `POST /api/v1/store/query` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    pub hits: Vec<QueryHit>,
}

/// `POST /api/v1/witness/verify` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WitnessVerifyResponse {
    pub valid: bool,
    pub entries: u64,
    pub head: String,
}

/// `GET /api/v1/custody/attestation` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationResponse {
    pub device_id: String,
    pub epoch: u64,
    pub vector_count: u64,
    pub witness_head: String,
    /// Ed25519 signature over `{epoch, vector_count, witness_head}`.
    pub signature: String,
}

/// `GET /api/v1/boundary` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundaryResponse {
    pub fragility: f32,
}
