//! Embedded vanilla-JS fleet UI served at `GET /`.
//!
//! Single page, no build step, no JS framework. Renders:
//!   * nodes table (autopolls /api/v1/nodes every 2 s)
//!   * cognitive headline (fragility / coherence from
//!     /api/v1/cognitive/snapshot)
//!   * live event log (subscribes to /api/v1/events via EventSource)
//!
//! The bearer token is cached in localStorage. First visit prompts you to
//! either click "pair" (works only when pairing is open) or paste an
//! existing token.

use axum::response::{Html, IntoResponse};

const UI_HTML: &str = include_str!("ui.html");

pub async fn handle_index() -> impl IntoResponse {
    Html(UI_HTML)
}
