//! `acorn-mcp` — MCP proxy: JSON-RPC 2.0 dispatcher + tool registry.
//!
//! Tools are namespaced `seed.<area>.<verb>` for wire compatibility with
//! upstream MCP consumers. This module ships a representative subset of the
//! ~114 tool surface; adding the rest is mechanical (more `registry.register`
//! calls — see [`default_registry`] for the pattern).
//!
//! Transport-agnostic: dispatch takes a [`JsonRpcRequest`] and returns a
//! [`JsonRpcResponse`]. `acorn-api` exposes this over HTTP at
//! `POST /api/v1/mcp`.

#![forbid(unsafe_code)]

use std::{collections::HashMap, sync::Arc};

use acorn_cognitive::Cognitive;
use acorn_proto::api::{IngestRequest, QueryRequest, VectorEntry};
use acorn_proto::rvf::RvfRecord;
use acorn_store::RvfStore;
use acorn_witness::{Custody, WitnessChain};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

impl JsonRpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

pub type ToolResult = Result<Value, JsonRpcError>;

// ---------------------------------------------------------------------------
// Tool trait + registry
// ---------------------------------------------------------------------------

pub trait ToolHandler: Send + Sync {
    fn call(&self, params: Value) -> ToolResult;
}

/// Convenience for closures.
impl<F> ToolHandler for F
where
    F: Fn(Value) -> ToolResult + Send + Sync,
{
    fn call(&self, params: Value) -> ToolResult {
        (self)(params)
    }
}

#[derive(Default)]
pub struct Registry {
    tools: HashMap<String, Arc<dyn ToolHandler>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<H: ToolHandler + 'static>(&mut self, name: impl Into<String>, h: H) {
        self.tools.insert(name.into(), Arc::new(h));
    }

    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.tools.keys().map(String::as_str).collect();
        v.sort();
        v
    }

    pub fn dispatch(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.unwrap_or(Value::Null);
        if req.jsonrpc != "2.0" {
            return err_resp(id, INVALID_REQUEST, "jsonrpc must be \"2.0\"");
        }
        // Built-in introspection.
        if req.method == "mcp.list_tools" {
            return ok_resp(id, json!({ "tools": self.names() }));
        }
        match self.tools.get(&req.method) {
            None => err_resp(id, METHOD_NOT_FOUND, format!("no such tool: {}", req.method)),
            Some(h) => match h.call(req.params) {
                Ok(v) => ok_resp(id, v),
                Err(e) => JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: None,
                    error: Some(e),
                },
            },
        }
    }
}

fn ok_resp(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn err_resp(id: Value, code: i32, msg: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError::new(code, msg)),
    }
}

// ---------------------------------------------------------------------------
// Default tool set
// ---------------------------------------------------------------------------

/// Runtime context shared by tool handlers.
#[derive(Clone)]
pub struct McpContext {
    pub store: Arc<RvfStore>,
    pub witness: Arc<WitnessChain>,
    pub custody: Arc<Custody>,
    pub cognitive: Arc<Cognitive>,
}

/// Register a representative subset of the `seed.*` tool surface. The
/// pattern is uniform — to add more, copy a `registry.register` line and
/// point the closure at the relevant runtime method.
pub fn default_registry(ctx: McpContext) -> Registry {
    let mut reg = Registry::new();

    // --- seed.memory.* -----------------------------------------------------
    {
        let ctx = ctx.clone();
        reg.register("seed.memory.ingest", move |params: Value| {
            let req: IngestRequest = serde_json::from_value(params)
                .map_err(|e| JsonRpcError::new(INVALID_PARAMS, e.to_string()))?;
            let ts32 = req.metadata.timestamp.clamp(0, u32::MAX as i64) as u32;
            let records: Vec<RvfRecord> = req
                .vectors
                .iter()
                .map(|VectorEntry(id, v)| RvfRecord {
                    id: *id,
                    vector: *v,
                    node_id: req.metadata.node_id,
                    type_tag: kind_to_tag(&req.metadata.kind),
                    timestamp: ts32,
                })
                .collect();
            for r in &records {
                ctx.witness
                    .append(&r.to_bytes())
                    .map_err(|e| JsonRpcError::new(INTERNAL_ERROR, e.to_string()))?;
            }
            let epoch = ctx
                .store
                .append_batch(&records)
                .map_err(|e| JsonRpcError::new(INTERNAL_ERROR, e.to_string()))?;
            Ok(json!({
                "accepted": records.len(),
                "epoch": epoch,
                "witness_head": hex::encode(ctx.witness.head()),
            }))
        });
    }
    {
        let ctx = ctx.clone();
        reg.register("seed.memory.query", move |params: Value| {
            let req: QueryRequest = serde_json::from_value(params)
                .map_err(|e| JsonRpcError::new(INVALID_PARAMS, e.to_string()))?;
            let hits = ctx.store.query_knn(&req.vector, req.k);
            Ok(json!({
                "hits": hits
                    .into_iter()
                    .map(|(id, distance)| json!({ "id": id, "distance": distance }))
                    .collect::<Vec<_>>(),
            }))
        });
    }
    {
        let ctx = ctx.clone();
        reg.register("seed.memory.export", move |_| {
            let bytes = ctx
                .store
                .export()
                .map_err(|e| JsonRpcError::new(INTERNAL_ERROR, e.to_string()))?;
            // Encode as hex; MCP clients can decode. Avoids needing a base64
            // dep in this crate.
            Ok(json!({ "rvf_hex": hex::encode(bytes) }))
        });
    }
    {
        let ctx = ctx.clone();
        reg.register("seed.memory.compact", move |_| {
            let n = ctx
                .store
                .compact()
                .map_err(|e| JsonRpcError::new(INTERNAL_ERROR, e.to_string()))?;
            Ok(json!({ "written": n }))
        });
    }

    // --- seed.witness.* ----------------------------------------------------
    {
        let ctx = ctx.clone();
        reg.register("seed.witness.verify", move |_| {
            let bytes = ctx.store.records_bytes();
            let refs: Vec<&[u8]> = bytes.iter().map(|v| v.as_slice()).collect();
            let result = ctx.witness.verify(refs.iter().copied());
            let valid = result.is_ok();
            let (entries, head) = result.unwrap_or_else(|_| (ctx.witness.count(), ctx.witness.head()));
            Ok(json!({
                "valid": valid,
                "entries": entries,
                "head": hex::encode(head),
            }))
        });
    }
    {
        let ctx = ctx.clone();
        reg.register("seed.witness.attestation", move |_| {
            let epoch = ctx.store.epoch();
            let count = ctx.store.count() as u64;
            let head = ctx.witness.head();
            let sig = ctx.custody.sign_attestation(epoch, count, &head);
            Ok(json!({
                "device_id": ctx.custody.device_id(),
                "epoch": epoch,
                "vector_count": count,
                "witness_head": hex::encode(head),
                "signature": hex::encode(sig),
            }))
        });
    }

    // --- seed.cognitive.* --------------------------------------------------
    // Both go through cached_or_compute so concurrent MCP traffic doesn't
    // multiply Stoer-Wagner runs (which would starve the tokio runtime).
    {
        let ctx = ctx.clone();
        reg.register("seed.cognitive.snapshot", move |_| {
            let store = ctx.store.clone();
            let snap = ctx
                .cognitive
                .cached_or_compute(|| store.vectors(), store.metric());
            Ok(serde_json::to_value(snap).unwrap())
        });
    }
    {
        let ctx = ctx.clone();
        reg.register("seed.cognitive.boundary", move |_| {
            let store = ctx.store.clone();
            let snap = ctx
                .cognitive
                .cached_or_compute(|| store.vectors(), store.metric());
            Ok(json!({ "fragility": snap.fragility }))
        });
    }

    // --- seed.sensor.* -----------------------------------------------------
    // Sensor tools depend on a live sensor runtime; without one wired in we
    // expose a placeholder that returns "no sensors bound" so consumers can
    // discover the namespace exists.
    reg.register("seed.sensor.snapshot", |_| {
        Ok(json!({
            "bound": false,
            "note": "no sensor runtime wired into mcp context"
        }))
    });

    reg
}

fn kind_to_tag(kind: &str) -> u8 {
    match kind {
        "csi_feature" => 1,
        "vitals" => 2,
        "raw_csi" => 3,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use acorn_cognitive::CognitiveConfig;
    use acorn_proto::rvf::Metric;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        p.push(format!("acorn-mcp-test-{pid}-{n}-{name}"));
        p
    }

    fn build_ctx() -> (McpContext, Vec<PathBuf>) {
        let rvf = temp_path("store.rvf");
        let wit = temp_path("witness.log");
        let key = temp_path("custody.key");
        let store = Arc::new(RvfStore::open_or_create(&rvf, Metric::Cosine).unwrap());
        let witness = Arc::new(WitnessChain::open(&wit).unwrap());
        let custody = Arc::new(Custody::load_or_create(&key).unwrap());
        let cognitive = Arc::new(Cognitive::new(CognitiveConfig::default()));
        let ctx = McpContext {
            store,
            witness,
            custody,
            cognitive,
        };
        (ctx, vec![rvf, wit, key])
    }

    fn req(method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        }
    }

    #[test]
    fn list_tools_returns_seed_namespaces() {
        let (ctx, files) = build_ctx();
        let reg = default_registry(ctx);
        let r = reg.dispatch(req("mcp.list_tools", json!(null)));
        let tools = r.result.unwrap()["tools"].as_array().unwrap().clone();
        let names: Vec<String> = tools.iter().map(|v| v.as_str().unwrap().into()).collect();
        for prefix in ["seed.memory.", "seed.witness.", "seed.cognitive.", "seed.sensor."] {
            assert!(names.iter().any(|n| n.starts_with(prefix)), "no {prefix}*");
        }
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    }

    #[test]
    fn ingest_query_via_mcp() {
        let (ctx, files) = build_ctx();
        let reg = default_registry(ctx);

        let ingest = reg.dispatch(req(
            "seed.memory.ingest",
            json!({
                "vectors": [[101, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]]],
                "metadata": {"node_id": 1, "type": "csi_feature", "timestamp": 0}
            }),
        ));
        assert!(ingest.error.is_none(), "{:?}", ingest.error);
        assert_eq!(ingest.result.unwrap()["accepted"], 1);

        let q = reg.dispatch(req(
            "seed.memory.query",
            json!({"vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], "k": 1}),
        ));
        let hits = q.result.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["id"], 101);

        for f in files {
            let _ = std::fs::remove_file(f);
        }
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let (ctx, files) = build_ctx();
        let reg = default_registry(ctx);
        let r = reg.dispatch(req("seed.bogus.thing", json!(null)));
        assert_eq!(r.error.as_ref().unwrap().code, METHOD_NOT_FOUND);
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    }

    #[test]
    fn rejects_wrong_jsonrpc_version() {
        let (ctx, files) = build_ctx();
        let reg = default_registry(ctx);
        let r = reg.dispatch(JsonRpcRequest {
            jsonrpc: "1.0".into(),
            id: Some(json!(7)),
            method: "mcp.list_tools".into(),
            params: json!(null),
        });
        assert_eq!(r.error.as_ref().unwrap().code, INVALID_REQUEST);
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    }
}
