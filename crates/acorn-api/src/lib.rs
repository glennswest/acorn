//! `acorn-api` — HTTPS endpoint surface for an `acornd` daemon.
//!
//! Phase 1 endpoints:
//!
//! | Method | Path | Auth |
//! |---|---|---|
//! | `POST` | `/api/v1/pair` | pairing window |
//! | `POST` | `/api/v1/pair/window` | none (TODO: USB-iface only) |
//! | `POST` | `/api/v1/store/ingest` | bearer |
//! | `POST` | `/api/v1/store/query`  | bearer |
//! | `GET`  | `/api/v1/store/export` | bearer |
//! | `POST` | `/api/v1/store/compact`| bearer |
//! | `POST` | `/api/v1/witness/verify` | bearer |
//! | `GET`  | `/api/v1/custody/attestation` | bearer |
//! | `GET`  | `/api/v1/system/health` | none |
//!
//! Bearer auth: `Authorization: Bearer <token>`. The token is compared via
//! SHA-256 against a per-instance hash issued by `/pair`. TLS is out of
//! scope for Phase 1 — terminate TLS in front (nginx, caddy, traefik) or add
//! a `rustls` listener in `acornd`. The bearer-only scheme is sufficient for
//! local-network testing against RuView's validator.

#![forbid(unsafe_code)]

pub mod events;
pub mod fleet;
pub mod ui;

pub use events::{EventBus, Webhook};
pub use fleet::{NodeRegistry, NodeState};

use std::{
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc,
    },
    time::SystemTime,
};

use acorn_cognitive::{Cognitive, CognitiveConfig};
use acorn_mcp::{JsonRpcRequest, McpContext, Registry};
use acorn_proto::api::{
    AttestationResponse, BoundaryResponse, CoherenceResponse, CognitiveSnapshotResponse,
    IngestRequest, IngestResponse, PairRequest, PairResponse, QueryHit, QueryRequest,
    QueryResponse, WitnessVerifyResponse,
};
use acorn_proto::rvf::RvfRecord;
use acorn_store::{RvfStore, StoreError};
use acorn_witness::{Custody, WitnessChain, WitnessError};
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use parking_lot::RwLock;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<RvfStore>,
    pub witness: Arc<WitnessChain>,
    pub custody: Arc<Custody>,
    pub auth: Arc<AuthState>,
    pub cognitive: Arc<Cognitive>,
    pub mcp: Arc<Registry>,
    pub swarm: Arc<SwarmState>,
    pub event_bus: Arc<EventBus>,
    pub nodes: Arc<NodeRegistry>,
    pub started_at: SystemTime,
    pub version: &'static str,
}

impl AppState {
    /// Default Cognitive config — convenience for tests and acornd.
    pub fn default_cognitive() -> Arc<Cognitive> {
        Arc::new(Cognitive::new(CognitiveConfig::default()))
    }

    /// Default MCP registry built from the runtime components.
    pub fn default_mcp_registry(
        store: Arc<RvfStore>,
        witness: Arc<WitnessChain>,
        custody: Arc<Custody>,
        cognitive: Arc<Cognitive>,
    ) -> Arc<Registry> {
        Arc::new(acorn_mcp::default_registry(McpContext {
            store,
            witness,
            custody,
            cognitive,
        }))
    }
}

/// In-memory swarm peer registry. Phase 4 scaffold — chain reconciliation
/// across peers is a follow-up.
#[derive(Default)]
pub struct SwarmState {
    peers: parking_lot::RwLock<Vec<Peer>>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Peer {
    pub id: String,
    pub url: String,
}

impl SwarmState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&self, peer: Peer) {
        let mut v = self.peers.write();
        if !v.iter().any(|p| p.id == peer.id) {
            v.push(peer);
        }
    }
    pub fn remove(&self, id: &str) -> bool {
        let mut v = self.peers.write();
        let before = v.len();
        v.retain(|p| p.id != id);
        v.len() != before
    }
    pub fn list(&self) -> Vec<Peer> {
        self.peers.read().clone()
    }
}

pub struct AuthState {
    token_hash: RwLock<Option<[u8; 32]>>,
    pairing_open: AtomicBool,
}

impl AuthState {
    pub fn new() -> Self {
        Self {
            token_hash: RwLock::new(None),
            pairing_open: AtomicBool::new(true),
        }
    }

    pub fn set_token_hash(&self, hash: [u8; 32]) {
        *self.token_hash.write() = Some(hash);
    }

    pub fn token_hash(&self) -> Option<[u8; 32]> {
        *self.token_hash.read()
    }

    pub fn open_pairing(&self) {
        self.pairing_open.store(true, AtomicOrdering::SeqCst);
    }

    pub fn close_pairing(&self) {
        self.pairing_open.store(false, AtomicOrdering::SeqCst);
    }

    pub fn pairing_open(&self) -> bool {
        self.pairing_open.load(AtomicOrdering::SeqCst)
    }
}

impl Default for AuthState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Error type / IntoResponse
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("pairing closed")]
    PairingClosed,
    #[error("not yet paired")]
    NotPaired,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("witness: {0}")]
    Witness(#[from] WitnessError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            ApiError::PairingClosed => (StatusCode::FORBIDDEN, self.to_string()),
            ApiError::NotPaired => (StatusCode::PRECONDITION_FAILED, self.to_string()),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            ApiError::Store(_) | ApiError::Witness(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Auth helper
// ---------------------------------------------------------------------------

fn require_bearer(headers: &HeaderMap, auth: &AuthState) -> Result<(), ApiError> {
    let expected = auth.token_hash().ok_or(ApiError::NotPaired)?;
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Unauthorized)?;
    let token = raw
        .strip_prefix("Bearer ")
        .ok_or(ApiError::Unauthorized)?
        .trim();
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let got: [u8; 32] = h.finalize().into();
    // Constant-time compare.
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= got[i] ^ expected[i];
    }
    if diff == 0 {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

/// Like [`require_bearer`] but also accepts a `?token=` query parameter,
/// for clients (EventSource) that can't set custom headers.
pub(crate) fn require_bearer_with_query(
    headers: &HeaderMap,
    query_token: Option<&str>,
    auth: &AuthState,
) -> Result<(), ApiError> {
    let expected = auth.token_hash().ok_or(ApiError::NotPaired)?;
    let token: &str = if let Some(raw) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        raw.trim()
    } else if let Some(t) = query_token {
        t.trim()
    } else {
        return Err(ApiError::Unauthorized);
    };
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let got: [u8; 32] = h.finalize().into();
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= got[i] ^ expected[i];
    }
    if diff == 0 {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui::handle_index))
        .route("/api/v1/pair", post(handle_pair))
        .route("/api/v1/pair/window", post(handle_pair_window))
        .route("/api/v1/store/ingest", post(handle_ingest))
        .route("/api/v1/store/query", post(handle_query))
        .route("/api/v1/store/export", get(handle_export))
        .route("/api/v1/store/compact", post(handle_compact))
        .route("/api/v1/witness/verify", post(handle_verify))
        .route("/api/v1/custody/attestation", get(handle_attestation))
        .route("/api/v1/boundary", get(handle_boundary))
        .route("/api/v1/coherence", get(handle_coherence))
        .route("/api/v1/cognitive/snapshot", get(handle_cognitive_snapshot))
        .route("/api/v1/mcp", post(handle_mcp))
        .route("/api/v1/swarm/peers", get(handle_swarm_list).post(handle_swarm_add))
        .route("/api/v1/swarm/peers/:id", axum::routing::delete(handle_swarm_remove))
        .route("/api/v1/swarm/sync", post(handle_swarm_sync))
        .route("/api/v1/events", get(events::handle_events_sse))
        .route("/api/v1/ws", get(events::handle_events_ws))
        .route(
            "/api/v1/webhooks",
            get(events::handle_webhooks_list).post(events::handle_webhooks_add),
        )
        .route(
            "/api/v1/webhooks/:id",
            axum::routing::delete(events::handle_webhooks_remove),
        )
        .route("/api/v1/nodes", get(fleet::handle_nodes_list))
        .route("/api/v1/nodes/:id", get(fleet::handle_node_get))
        .route("/api/v1/system/health", get(handle_health))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_pair(
    State(state): State<AppState>,
    Json(req): Json<PairRequest>,
) -> Result<Json<PairResponse>, ApiError> {
    if !state.auth.pairing_open() {
        return Err(ApiError::PairingClosed);
    }
    if req.client_name.trim().is_empty() {
        return Err(ApiError::BadRequest("client_name required".into()));
    }

    let mut token_bytes = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut token_bytes[..]);
    let token = hex::encode(token_bytes);
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let token_hash: [u8; 32] = h.finalize().into();
    state.auth.set_token_hash(token_hash);
    state.auth.close_pairing();

    Ok(Json(PairResponse {
        token,
        device_id: state.custody.device_id(),
    }))
}

async fn handle_pair_window(State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    // TODO: enforce USB-iface restriction once the daemon supports listing
    // bound interfaces. For now, opening the window is unauthenticated.
    state.auth.open_pairing();
    Ok(StatusCode::NO_CONTENT)
}

async fn handle_ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IngestRequest>,
) -> Result<Json<IngestResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;

    // Translate proto VectorEntry + metadata -> RvfRecord.
    let ts32 = req
        .metadata
        .timestamp
        .clamp(0, u32::MAX as i64) as u32;
    let mut records: Vec<RvfRecord> = Vec::with_capacity(req.vectors.len());
    for v in req.vectors.iter() {
        records.push(RvfRecord {
            id: v.0,
            vector: v.1,
            node_id: req.metadata.node_id,
            type_tag: kind_to_tag(&req.metadata.kind),
            timestamp: ts32,
        });
    }

    let mut head = state.witness.head();
    for r in &records {
        head = state.witness.append(&r.to_bytes())?;
    }
    let epoch = state.store.append_batch(&records)?;

    Ok(Json(IngestResponse {
        accepted: records.len(),
        epoch,
        witness_head: hex::encode(head),
    }))
}

fn kind_to_tag(kind: &str) -> u8 {
    match kind {
        "csi_feature" => 1,
        "vitals" => 2,
        "raw_csi" => 3,
        _ => 0,
    }
}

async fn handle_query(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let hits = state
        .store
        .query_knn(&req.vector, req.k)
        .into_iter()
        .map(|(id, distance)| QueryHit { id, distance })
        .collect();
    Ok(Json(QueryResponse { hits }))
}

async fn handle_export(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let bytes = state.store.export()?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        Bytes::from(bytes),
    )
        .into_response())
}

#[derive(Serialize)]
struct CompactResponse {
    written: usize,
}

async fn handle_compact(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CompactResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let written = state.store.compact()?;
    Ok(Json(CompactResponse { written }))
}

async fn handle_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<WitnessVerifyResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let bytes = state.store.records_bytes();
    let refs: Vec<&[u8]> = bytes.iter().map(|v| v.as_slice()).collect();
    let result = state.witness.verify(refs.iter().copied());
    let valid = result.is_ok();
    let (entries, head) = result.unwrap_or_else(|_| (state.witness.count(), state.witness.head()));
    Ok(Json(WitnessVerifyResponse {
        valid,
        entries,
        head: hex::encode(head),
    }))
}

async fn handle_attestation(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AttestationResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let epoch = state.store.epoch();
    let count = state.store.count() as u64;
    let head = state.witness.head();
    let sig = state.custody.sign_attestation(epoch, count, &head);
    Ok(Json(AttestationResponse {
        device_id: state.custody.device_id(),
        epoch,
        vector_count: count,
        witness_head: hex::encode(head),
        signature: hex::encode(sig),
    }))
}

async fn handle_boundary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<BoundaryResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let vectors = state.store.vectors();
    let snap = state.cognitive.snapshot(&vectors, state.store.metric());
    Ok(Json(BoundaryResponse {
        fragility: snap.fragility,
    }))
}

async fn handle_coherence(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CoherenceResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let vectors = state.store.vectors();
    let snap = state.cognitive.snapshot(&vectors, state.store.metric());
    Ok(Json(CoherenceResponse {
        coherence: snap.coherence,
    }))
}

async fn handle_cognitive_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CognitiveSnapshotResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let vectors = state.store.vectors();
    let snap = state.cognitive.snapshot(&vectors, state.store.metric());
    Ok(Json(CognitiveSnapshotResponse {
        vector_count: snap.vector_count,
        fragility: snap.fragility,
        coherence: snap.coherence,
        min_cut: snap.min_cut,
        k_neighbors: snap.k_neighbors,
        coherence_window: snap.coherence_window,
    }))
}

#[derive(Serialize)]
struct HealthResponse {
    version: &'static str,
    device_id: String,
    paired: bool,
    pairing_open: bool,
    store_count: usize,
    witness_count: u64,
    uptime_secs: u64,
}

async fn handle_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let resp = state.mcp.dispatch(req);
    Ok(Json(serde_json::to_value(resp).unwrap()))
}

async fn handle_swarm_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Peer>>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    Ok(Json(state.swarm.list()))
}

async fn handle_swarm_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(peer): Json<Peer>,
) -> Result<StatusCode, ApiError> {
    require_bearer(&headers, &state.auth)?;
    if peer.id.trim().is_empty() || peer.url.trim().is_empty() {
        return Err(ApiError::BadRequest("id and url required".into()));
    }
    state.swarm.add(peer);
    Ok(StatusCode::NO_CONTENT)
}

async fn handle_swarm_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, ApiError> {
    require_bearer(&headers, &state.auth)?;
    if state.swarm.remove(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
}

#[derive(serde::Deserialize)]
struct SyncRequest {
    peer_epoch: u64,
    peer_head: String,
}

#[derive(Serialize)]
struct SyncResponse {
    local_epoch: u64,
    local_count: u64,
    local_head: String,
    /// "ahead" — we have a higher epoch; peer should pull from us.
    /// "behind" — peer has a higher epoch; we'd pull from them (TODO).
    /// "synced" — same epoch.
    relation: &'static str,
}

async fn handle_swarm_sync(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let local_epoch = state.store.epoch();
    let local_count = state.witness.count();
    let local_head = hex::encode(state.witness.head());
    let relation = if local_epoch > req.peer_epoch {
        "ahead"
    } else if local_epoch < req.peer_epoch {
        // Pulling deltas from a higher-epoch peer with witness-chain
        // integrity is non-trivial — left as a follow-up.
        tracing::info!(
            local = local_epoch,
            remote = req.peer_epoch,
            peer_head = %req.peer_head,
            "swarm pull TODO"
        );
        "behind"
    } else {
        "synced"
    };
    Ok(Json(SyncResponse {
        local_epoch,
        local_count,
        local_head,
        relation,
    }))
}

async fn handle_health(State(state): State<AppState>) -> Json<HealthResponse> {
    let uptime = SystemTime::now()
        .duration_since(state.started_at)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Json(HealthResponse {
        version: state.version,
        device_id: state.custody.device_id(),
        paired: state.auth.token_hash().is_some(),
        pairing_open: state.auth.pairing_open(),
        store_count: state.store.count(),
        witness_count: state.witness.count(),
        uptime_secs: uptime,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use acorn_proto::api::{IngestMetadata, VectorEntry};
    use acorn_proto::rvf::Metric;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    fn temp_path(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        p.push(format!("acorn-api-test-{pid}-{n}-{name}"));
        p
    }

    fn build_state() -> (AppState, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let rvf = temp_path("store.rvf");
        let wit = temp_path("witness.log");
        let key = temp_path("custody.key");
        let store = Arc::new(RvfStore::open_or_create(&rvf, Metric::Cosine).unwrap());
        let witness = Arc::new(WitnessChain::open(&wit).unwrap());
        let custody = Arc::new(Custody::load_or_create(&key).unwrap());
        let cognitive = AppState::default_cognitive();
        let mcp = AppState::default_mcp_registry(
            store.clone(),
            witness.clone(),
            custody.clone(),
            cognitive.clone(),
        );
        let state = AppState {
            store,
            witness,
            custody,
            auth: Arc::new(AuthState::new()),
            cognitive,
            mcp,
            swarm: Arc::new(SwarmState::new()),
            event_bus: Arc::new(EventBus::new()),
            nodes: Arc::new(NodeRegistry::new()),
            started_at: SystemTime::now(),
            version: "test",
        };
        (state, rvf, wit, key)
    }

    async fn body_json<T: serde::de::DeserializeOwned>(resp: Response) -> T {
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn pair_then_ingest_then_query_then_verify() {
        let (state, rvf, wit, key) = build_state();
        let app = router(state.clone());

        // Pair
        let pair: PairResponse = body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/pair")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&PairRequest {
                                client_name: "tester".into(),
                            })
                            .unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let token = pair.token;
        assert!(!token.is_empty());

        // Pairing should now be closed.
        let pair_again = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/pair")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&PairRequest {
                            client_name: "x".into(),
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pair_again.status(), StatusCode::FORBIDDEN);

        // Ingest
        let ingest_body = IngestRequest {
            vectors: vec![
                VectorEntry(1, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                VectorEntry(2, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            ],
            metadata: IngestMetadata {
                node_id: 1,
                kind: "csi_feature".into(),
                timestamp: 123,
            },
        };
        let ingest: IngestResponse = body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/store/ingest")
                        .header("content-type", "application/json")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::from(serde_json::to_vec(&ingest_body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(ingest.accepted, 2);
        assert_eq!(ingest.epoch, 1);

        // Query
        let q: QueryResponse = body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/store/query")
                        .header("content-type", "application/json")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::from(
                            serde_json::to_vec(&QueryRequest {
                                vector: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                                k: 1,
                            })
                            .unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(q.hits.len(), 1);
        assert_eq!(q.hits[0].id, 1);

        // Witness verify
        let v: WitnessVerifyResponse = body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/witness/verify")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert!(v.valid);
        assert_eq!(v.entries, 2);

        // Attestation
        let a: AttestationResponse = body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v1/custody/attestation")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(a.vector_count, 2);

        let _ = std::fs::remove_file(&rvf);
        let _ = std::fs::remove_file(&wit);
        let _ = std::fs::remove_file(&key);
    }

    #[tokio::test]
    async fn unauthorized_ingest_rejected() {
        let (state, rvf, wit, key) = build_state();
        let app = router(state);
        // Pair so a token exists; then call ingest without auth.
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/pair")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&PairRequest {
                            client_name: "x".into(),
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/store/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from("{\"vectors\":[],\"metadata\":{\"node_id\":1,\"type\":\"csi_feature\",\"timestamp\":0}}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_file(&rvf);
        let _ = std::fs::remove_file(&wit);
        let _ = std::fs::remove_file(&key);
    }

    #[tokio::test]
    async fn health_is_public() {
        let (state, rvf, wit, key) = build_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/system/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = std::fs::remove_file(&rvf);
        let _ = std::fs::remove_file(&wit);
        let _ = std::fs::remove_file(&key);
    }
}
