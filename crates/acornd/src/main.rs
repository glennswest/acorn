//! `acornd` — wire-compatible Cognitum Seed daemon.
//!
//! Wiring:
//!   UDP :5006 (feature packets) -> acorn-store + acorn-witness  (mod ingest)
//!   HTTP :8443 (RuView surface) -> acorn-api
//!
//! TLS is intentionally out of scope for the initial daemon; terminate TLS
//! in front (caddy/nginx/traefik) or add a rustls listener here later.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::SystemTime};

use acorn_api::{router, AppState, AuthState};
use acorn_proto::rvf::Metric;
use acorn_store::RvfStore;
use acorn_witness::{Custody, WitnessChain};
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod ingest;

#[derive(Parser, Debug)]
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

    let state = AppState {
        store: store.clone(),
        witness: witness.clone(),
        custody: custody.clone(),
        auth,
        cognitive: AppState::default_cognitive(),
        started_at: SystemTime::now(),
        version: env!("CARGO_PKG_VERSION"),
    };

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

    let udp_handle = tokio::spawn(ingest::run(args.udp_addr, store, witness));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("ctrl-c received, shutting down");
        }
        r = http_handle => tracing::warn!(?r, "http task exited"),
        r = udp_handle  => tracing::warn!(?r, "udp ingest task exited"),
    }
    Ok(())
}
