//! MQTT publish fan-out.
//!
//! Subscribes to the in-process `EventBus.sensing` channel and publishes
//! each [`SensingEvent`] as JSON to
//! `<topic_prefix>/<kind>/<zone>` (e.g. `acorn/events/occupancy/node-42`).
//! QoS-1, retain off. Reconnect handled by rumqttc's event loop.

use std::{sync::Arc, time::Duration};

use acorn_proto::event::SensingEvent;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use tokio::sync::broadcast;

use crate::EventBus;

/// Connection knobs. None of these are sensitive enough to want hidden in
/// some external config — they're all flag-or-env.
#[derive(Debug, Clone)]
pub struct MqttConfig {
    /// Broker URL, e.g. `mqtt://zman.g9.lo:1883`.
    pub url: String,
    /// Topic prefix; the kind and zone are appended.
    pub topic_prefix: String,
    /// Optional client id; defaults to `acorn-<hostname>`.
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Parse a `mqtt://[user[:pass]@]host[:port]` URL into a [`MqttOptions`]
/// plus optional user/pass override (CLI/env can also supply credentials).
pub fn build_options(cfg: &MqttConfig) -> Result<MqttOptions, String> {
    let parsed = url::Url::parse(&cfg.url).map_err(|e| format!("mqtt url parse: {e}"))?;
    let scheme = parsed.scheme();
    if scheme != "mqtt" && scheme != "mqtts" {
        return Err(format!("mqtt url scheme must be mqtt://, got {scheme}://"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "mqtt url missing host".to_string())?;
    let port = parsed.port().unwrap_or(1883);
    let mut opts = MqttOptions::new(&cfg.client_id, host, port);
    opts.set_keep_alive(Duration::from_secs(30));
    // Username/password precedence: explicit CLI, then URL.
    let user = cfg
        .username
        .clone()
        .or_else(|| Some(parsed.username().to_string()).filter(|s| !s.is_empty()));
    let pass = cfg
        .password
        .clone()
        .or_else(|| parsed.password().map(|s| s.to_string()));
    if let (Some(u), Some(p)) = (user.as_ref(), pass.as_ref()) {
        opts.set_credentials(u, p);
    } else if let Some(u) = user.as_ref() {
        opts.set_credentials(u, "");
    }
    Ok(opts)
}

/// Spawn the publisher. Returns the eventloop driver join handle.
///
/// If the broker is unreachable, rumqttc retries every 5 s (its default).
/// Publishes are fire-and-forget at QoS-1 — buffering happens inside
/// `AsyncClient`; if the queue fills up we drop the event and log a warn.
pub fn spawn_mqtt_publisher(
    cfg: MqttConfig,
    bus: Arc<EventBus>,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let opts = match build_options(&cfg) {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(%e, "mqtt config invalid; publisher disabled");
            // Return two no-op tasks so the caller has a single shape to await.
            return (tokio::spawn(async {}), tokio::spawn(async {}));
        }
    };
    let (client, mut eventloop) = AsyncClient::new(opts, 256);
    let prefix = cfg.topic_prefix.trim_end_matches('/').to_string();
    tracing::info!(broker = %cfg.url, topic_prefix = %prefix, "mqtt publisher starting");

    // Driver task: poll the eventloop forever so rumqttc actually sends.
    let driver = tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(notif) => tracing::trace!(?notif, "mqtt"),
                Err(e) => {
                    tracing::warn!(?e, "mqtt eventloop error; backing off 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });

    // Fan-out task: subscribe to the bus and publish each event.
    let publisher = tokio::spawn(async move {
        let mut rx = bus.sensing.subscribe();
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let topic = format!(
                        "{}/{}/{}",
                        prefix,
                        event_kind(&ev),
                        sanitize_zone(event_zone(&ev))
                    );
                    let body = match serde_json::to_vec(&ev) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    if let Err(e) = client.publish(topic.clone(), QoS::AtLeastOnce, false, body).await {
                        tracing::warn!(%topic, ?e, "mqtt publish failed");
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "mqtt fan-out lagged");
                }
            }
        }
    });

    (driver, publisher)
}

fn event_kind(ev: &SensingEvent) -> &'static str {
    match ev {
        SensingEvent::Occupancy { .. } => "occupancy",
        SensingEvent::Motion { .. } => "motion",
        SensingEvent::Fall { .. } => "fall",
        SensingEvent::Vitals { .. } => "vitals",
        SensingEvent::RegimeChange { .. } => "regime_change",
    }
}

fn event_zone(ev: &SensingEvent) -> &str {
    match ev {
        SensingEvent::Occupancy { zone, .. }
        | SensingEvent::Motion { zone, .. }
        | SensingEvent::Fall { zone }
        | SensingEvent::Vitals { zone, .. }
        | SensingEvent::RegimeChange { zone, .. } => zone.as_str(),
    }
}

/// Replace MQTT-special characters in a zone string before using it as a
/// topic segment. `/`, `+`, `#`, control chars become `_`.
fn sanitize_zone(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '+' | '#' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}
