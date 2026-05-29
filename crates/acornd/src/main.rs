//! `acornd` — wire-compatible Cognitum Seed daemon.
//!
//! Wiring:
//!   UDP :5006 (feature packets) -> store + witness + reflex + bus + nodes
//!   HTTP :8443 (RuView surface) -> acorn-api (REST + SSE + WS + MCP + UI)
//!   Webhook fan-out task        -> POSTs sensing events to registered URLs
//!
//! There is intentionally no local sensor pipeline: sensors are distributed
//! ESP32 nodes that push feature packets to the UDP port. TLS is out of
//! scope for the daemon itself — terminate TLS in front (caddy/nginx) or
//! add a rustls listener here later.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::SystemTime};

use acorn_api::{events::spawn_webhook_fanout, router, AppState, AuthState, EventBus, NodeRegistry, SwarmState};
use acorn_proto::rvf::Metric;
use acorn_sensors::{Reflex, ReflexConfig};
use acorn_store::RvfStore;
use acorn_witness::{Custody, WitnessChain};
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod ingest;

#[derive(Parser, Debug, Clone)]
#[command(version, about = "Wire-compatible Cognitum Seed daemon")]
struct Args {
    /// HTTP listen address.
    #[arg(long, env = "ACORND_HTTP_ADDR", default_value = "0.0.0.0:8443")]
    http_addr: SocketAddr,

    /// UDP feature-packet listen address (ADR-069 specifies port 5006).
    #[arg(long, env = "ACORND_UDP_ADDR", default_value = "0.0.0.0:5006")]
    udp_addr: SocketAddr,

    /// Path to the RVF store file.
    #[arg(long, env = "ACORND_STORE", default_value = "acorn-store.rvf")]
    store: PathBuf,

    /// Path to the witness chain file.
    #[arg(long, env = "ACORND_WITNESS", default_value = "acorn-witness.log")]
    witness: PathBuf,

    /// Path to the Ed25519 device-custody key.
    #[arg(long, env = "ACORND_CUSTODY", default_value = "acorn-custody.key")]
    custody: PathBuf,

    /// Distance metric: cosine | l2 | dot.
    #[arg(long, env = "ACORND_METRIC", default_value = "cosine", value_parser = parse_metric)]
    metric: Metric,

    /// Presence-detection threshold (0..1) for the CSI feature vector.
    #[arg(long, env = "ACORND_PRESENCE_THRESHOLD", default_value_t = 0.5)]
    presence_threshold: f32,

    /// Motion-energy threshold (0..1).
    #[arg(long, env = "ACORND_MOTION_THRESHOLD", default_value_t = 0.7)]
    motion_threshold: f32,
}

fn parse_metric(s: &str) -> Result<Metric, String> {
    match s.to_ascii_lowercase().as_str() {
        "cosine" => Ok(Metric::Cosine),
        "l2" => Ok(Metric::L2),
        "dot" => Ok(Metric::Dot),
        other => Err(format!("unknown metric: {other} (want cosine|l2|dot)")),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let store = Arc::new(RvfStore::open_or_create(&args.store, args.metric)?);
    let witness = Arc::new(WitnessChain::open(&args.witness)?);
    let custody = Arc::new(Custody::load_or_create(&args.custody)?);
    let auth = Arc::new(AuthState::new());

    let cognitive = AppState::default_cognitive();
    let mcp = AppState::default_mcp_registry(
        store.clone(),
        witness.clone(),
        custody.clone(),
        cognitive.clone(),
    );
    let event_bus = Arc::new(EventBus::new());
    let nodes = Arc::new(NodeRegistry::new());

    let state = AppState {
        store: store.clone(),
        witness: witness.clone(),
        custody: custody.clone(),
        auth,
        cognitive,
        mcp,
        swarm: Arc::new(SwarmState::new()),
        event_bus: event_bus.clone(),
        nodes: nodes.clone(),
        started_at: SystemTime::now(),
        version: env!("CARGO_PKG_VERSION"),
    };

    let reflex = Arc::new(Reflex::new(ReflexConfig {
        presence_threshold: args.presence_threshold,
        motion_threshold: args.motion_threshold,
        ..ReflexConfig::default()
    }));

    tracing::info!(
        device_id = %custody.device_id(),
        http = %args.http_addr,
        udp = %args.udp_addr,
        "acornd starting"
    );

    let listener = tokio::net::TcpListener::bind(args.http_addr).await?;
    let app = router(state);

    let http_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(?e, "axum serve exited");
        }
    });

    let udp_handle = tokio::spawn(ingest::run(
        args.udp_addr,
        store,
        witness,
        reflex,
        event_bus.clone(),
        nodes,
    ));

    let _webhook_handle = spawn_webhook_fanout(event_bus.clone());

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("ctrl-c received, shutting down");
        }
        r = http_handle => tracing::warn!(?r, "http task exited"),
        r = udp_handle  => tracing::warn!(?r, "udp ingest task exited"),
    }
    Ok(())
}
