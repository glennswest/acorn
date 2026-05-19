//! `acorn-client` — typed async HTTP client for an `acornd` daemon.
//!
//! Z Man imports this crate + [`acorn_proto`] and nothing else.
//! Intentionally free of appliance-internal crates so the binary surface a
//! consumer pulls in is small.

#![forbid(unsafe_code)]

use acorn_proto::api::{
    AttestationResponse, IngestRequest, IngestResponse, PairRequest, PairResponse, QueryRequest,
    QueryResponse, WitnessVerifyResponse,
};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api {status}: {body}")]
    Api { status: u16, body: String },
    #[error("invalid header value")]
    InvalidHeader,
}

/// Typed client over the Acorn HTTP surface.
#[derive(Clone)]
pub struct AcornClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl AcornClient {
    /// Construct a new client targeting `base` (e.g. `http://10.0.0.5:8443`).
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.into(),
            token: None,
        }
    }

    /// Construct with an already-issued token (e.g. after a previous pair).
    pub fn with_token(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.into(),
            token: Some(token.into()),
        }
    }

    /// Set / replace the bearer token used by authenticated calls.
    pub fn set_token(&mut self, token: impl Into<String>) {
        self.token = Some(token.into());
    }

    /// `POST /api/v1/pair`. On success, stores the token internally and
    /// returns the response (containing token + device_id).
    pub async fn pair(&mut self, client_name: impl Into<String>) -> Result<PairResponse, ClientError> {
        let req = PairRequest {
            client_name: client_name.into(),
        };
        let resp: PairResponse = self.post_json("/api/v1/pair", &req, false).await?;
        self.token = Some(resp.token.clone());
        Ok(resp)
    }

    /// `POST /api/v1/store/ingest`.
    pub async fn ingest(&self, req: &IngestRequest) -> Result<IngestResponse, ClientError> {
        self.post_json("/api/v1/store/ingest", req, true).await
    }

    /// `POST /api/v1/store/query`.
    pub async fn query(&self, req: &QueryRequest) -> Result<QueryResponse, ClientError> {
        self.post_json("/api/v1/store/query", req, true).await
    }

    /// `GET /api/v1/store/export` — returns the raw RVF file bytes.
    pub async fn export(&self) -> Result<Vec<u8>, ClientError> {
        let url = format!("{}/api/v1/store/export", self.base);
        let mut headers = HeaderMap::new();
        self.attach_auth(&mut headers)?;
        let resp = self.http.get(&url).headers(headers).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// `POST /api/v1/store/compact`.
    pub async fn compact(&self) -> Result<usize, ClientError> {
        #[derive(Deserialize)]
        struct CompactResponse {
            written: usize,
        }
        let r: CompactResponse = self.post_empty("/api/v1/store/compact", true).await?;
        Ok(r.written)
    }

    /// `POST /api/v1/witness/verify`.
    pub async fn verify_witness(&self) -> Result<WitnessVerifyResponse, ClientError> {
        self.post_empty("/api/v1/witness/verify", true).await
    }

    /// `GET /api/v1/custody/attestation`.
    pub async fn attestation(&self) -> Result<AttestationResponse, ClientError> {
        self.get_json("/api/v1/custody/attestation", true).await
    }

    /// `GET /api/v1/system/health`.
    pub async fn health(&self) -> Result<serde_json::Value, ClientError> {
        self.get_json("/api/v1/system/health", false).await
    }

    // ---------- private helpers ----------

    fn attach_auth(&self, headers: &mut HeaderMap) -> Result<(), ClientError> {
        if let Some(t) = &self.token {
            let v = HeaderValue::from_str(&format!("Bearer {t}"))
                .map_err(|_| ClientError::InvalidHeader)?;
            headers.insert(AUTHORIZATION, v);
        }
        Ok(())
    }

    async fn post_json<B: serde::Serialize + ?Sized, R: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
        auth: bool,
    ) -> Result<R, ClientError> {
        let url = format!("{}{}", self.base, path);
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if auth {
            self.attach_auth(&mut headers)?;
        }
        let resp = self.http.post(&url).headers(headers).json(body).send().await?;
        handle(resp).await
    }

    async fn post_empty<R: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        auth: bool,
    ) -> Result<R, ClientError> {
        let url = format!("{}{}", self.base, path);
        let mut headers = HeaderMap::new();
        if auth {
            self.attach_auth(&mut headers)?;
        }
        let resp = self.http.post(&url).headers(headers).send().await?;
        handle(resp).await
    }

    async fn get_json<R: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        auth: bool,
    ) -> Result<R, ClientError> {
        let url = format!("{}{}", self.base, path);
        let mut headers = HeaderMap::new();
        if auth {
            self.attach_auth(&mut headers)?;
        }
        let resp = self.http.get(&url).headers(headers).send().await?;
        handle(resp).await
    }
}

async fn handle<R: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<R, ClientError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ClientError::Api {
            status: status.as_u16(),
            body,
        });
    }
    Ok(resp.json().await?)
}

// ---------------------------------------------------------------------------
// Integration test: in-process router + reqwest
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use acorn_api::{router, AppState, AuthState};
    use acorn_proto::api::{IngestMetadata, VectorEntry};
    use acorn_proto::rvf::Metric;
    use acorn_store::RvfStore;
    use acorn_witness::{Custody, WitnessChain};
    use std::{sync::Arc, time::SystemTime};

    fn temp_path(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        p.push(format!("acorn-client-test-{pid}-{n}-{name}"));
        p
    }

    async fn spawn_server() -> (String, tokio::task::JoinHandle<()>, Vec<std::path::PathBuf>) {
        let rvf = temp_path("store.rvf");
        let wit = temp_path("witness.log");
        let key = temp_path("custody.key");
        let store = Arc::new(RvfStore::open_or_create(&rvf, Metric::Cosine).unwrap());
        let witness = Arc::new(WitnessChain::open(&wit).unwrap());
        let custody = Arc::new(Custody::load_or_create(&key).unwrap());
        let state = AppState {
            store,
            witness,
            custody,
            auth: Arc::new(AuthState::new()),
            started_at: SystemTime::now(),
            version: "test",
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle, vec![rvf, wit, key])
    }

    #[tokio::test]
    async fn full_client_roundtrip() {
        let (base, server, files) = spawn_server().await;
        let mut client = AcornClient::new(&base);

        // Health works without auth.
        let h = client.health().await.unwrap();
        assert_eq!(h["paired"], false);

        // Pair.
        let p = client.pair("integration-test").await.unwrap();
        assert!(!p.token.is_empty());

        // Ingest.
        let ingest = client
            .ingest(&IngestRequest {
                vectors: vec![
                    VectorEntry(11, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                    VectorEntry(22, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                ],
                metadata: IngestMetadata {
                    node_id: 1,
                    kind: "csi_feature".into(),
                    timestamp: 1_700_000_000,
                },
            })
            .await
            .unwrap();
        assert_eq!(ingest.accepted, 2);
        assert_eq!(ingest.epoch, 1);

        // Query.
        let q = client
            .query(&QueryRequest {
                vector: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                k: 1,
            })
            .await
            .unwrap();
        assert_eq!(q.hits.len(), 1);
        assert_eq!(q.hits[0].id, 11);

        // Verify.
        let v = client.verify_witness().await.unwrap();
        assert!(v.valid);
        assert_eq!(v.entries, 2);

        // Attestation.
        let a = client.attestation().await.unwrap();
        assert_eq!(a.vector_count, 2);
        assert_eq!(a.epoch, 1);

        // Export returns at least the 32-byte header with the magic.
        let bytes = client.export().await.unwrap();
        assert!(bytes.len() >= 32);
        assert_eq!(&bytes[0..4], b"RVF1");

        server.abort();
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    }
}
