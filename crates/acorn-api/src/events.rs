//! Event bus + SSE + WebSocket + webhook fan-out.
//!
//! The bus carries [`SensingEvent`]s produced by [`acorn_sensors::Reflex`]
//! over each ESP32 feature packet. Consumers subscribe via:
//!
//! * `GET  /api/v1/events`      — Server-Sent Events stream
//! * `GET  /api/v1/ws`          — WebSocket upgrade
//! * `GET  /api/v1/webhooks`    — list registered webhook URLs
//! * `POST /api/v1/webhooks`    — register a new webhook URL
//! * `DELETE /api/v1/webhooks/:id` — unregister
//!
//! All require bearer auth.

use std::{convert::Infallible, sync::Arc, time::Duration};

use acorn_proto::event::SensingEvent;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::{require_bearer, ApiError, AppState};

const BUS_CAPACITY: usize = 256;

/// In-process event bus: a broadcast channel for [`SensingEvent`]s plus a
/// registry of outbound webhook URLs. The fan-out task
/// ([`spawn_webhook_fanout`]) subscribes and POSTs each event to every
/// registered URL.
pub struct EventBus {
    pub sensing: broadcast::Sender<SensingEvent>,
    pub webhooks: RwLock<Vec<Webhook>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            sensing: broadcast::channel(BUS_CAPACITY).0,
            webhooks: RwLock::new(Vec::new()),
        }
    }

    pub fn publish_sensing(&self, ev: SensingEvent) {
        let _ = self.sensing.send(ev);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookCreate {
    pub url: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TokenQuery {
    #[serde(default)]
    token: Option<String>,
}

pub async fn handle_events_sse(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // EventSource can't set custom headers, so accept ?token=... too.
    crate::require_bearer_with_query(&headers, q.token.as_deref(), &state.auth)?;
    let rx = state.event_bus.sensing.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| async move {
        match res {
            Ok(ev) => {
                let payload = serde_json::to_string(&ev).ok()?;
                Some(Ok::<_, Infallible>(Event::default().data(payload)))
            }
            Err(_lagged) => None,
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

pub async fn handle_events_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = require_bearer(&headers, &state.auth) {
        return e.into_response();
    }
    let bus = state.event_bus.clone();
    ws.on_upgrade(move |socket| async move {
        run_ws(socket, bus).await;
    })
}

async fn run_ws(socket: WebSocket, bus: Arc<EventBus>) {
    let (mut tx, mut rx_ws) = socket.split();
    let mut rx_ev = bus.sensing.subscribe();
    loop {
        tokio::select! {
            ev = rx_ev.recv() => {
                match ev {
                    Ok(ev) => {
                        let body = match serde_json::to_string(&ev) {
                            Ok(b) => b,
                            Err(_) => continue,
                        };
                        if tx.send(Message::Text(body)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            msg = rx_ws.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

pub async fn handle_webhooks_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Webhook>>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    Ok(Json(state.event_bus.webhooks.read().clone()))
}

pub async fn handle_webhooks_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<WebhookCreate>,
) -> Result<Json<Webhook>, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let url = req.url.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::BadRequest("url must start with http(s)://".into()));
    }
    let id = next_webhook_id();
    let wh = Webhook { id: id.clone(), url };
    state.event_bus.webhooks.write().push(wh.clone());
    Ok(Json(wh))
}

pub async fn handle_webhooks_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_bearer(&headers, &state.auth)?;
    let mut w = state.event_bus.webhooks.write();
    let before = w.len();
    w.retain(|h| h.id != id);
    if w.len() == before {
        Ok(StatusCode::NOT_FOUND)
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

fn next_webhook_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    format!("wh-{}", N.fetch_add(1, Ordering::SeqCst))
}

// ---------------------------------------------------------------------------
// Webhook fan-out task
// ---------------------------------------------------------------------------

/// Spawn the background task that mirrors `EventBus.sensing` to every
/// registered webhook URL via fire-and-forget HTTP POSTs.
pub fn spawn_webhook_fanout(bus: Arc<EventBus>) -> tokio::task::JoinHandle<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut rx = bus.sensing.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let urls: Vec<String> = bus
                        .webhooks
                        .read()
                        .iter()
                        .map(|h| h.url.clone())
                        .collect();
                    if urls.is_empty() {
                        continue;
                    }
                    let body = match serde_json::to_value(&ev) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    for url in urls {
                        let c = client.clone();
                        let b = body.clone();
                        tokio::spawn(async move {
                            if let Err(e) = c.post(&url).json(&b).send().await {
                                tracing::warn!(%url, ?e, "webhook POST failed");
                            }
                        });
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "webhook fan-out lagged");
                }
            }
        }
    })
}
