//! `acorn-proto` — wire types, RVF format, HTTP DTOs and the sensing-event
//! vocabulary for a wire-compatible Cognitum Seed reimplementation.
//!
//! This crate is the **integration boundary**. It is intentionally
//! dependency-light (only `serde`) so it can be imported by Z Man — or any
//! other consumer — without pulling in appliance internals (`acorn-store`,
//! `acorn-api`, `acorn-sensors`, ...).
//!
//! Module map:
//! - [`udp`]   — the ESP32 -> Seed UDP packets (raw CSI / vitals / feature).
//! - [`rvf`]   — RuVector Format records. **Layout is provisional** — see notes.
//! - [`api`]   — the RuView-facing HTTP JSON request/response DTOs.
//! - [`event`] — semantic sensing events; the Z Man-facing vocabulary.
//! - [`error`] — [`ProtoError`].

#![forbid(unsafe_code)]

pub mod api;
pub mod error;
pub mod event;
pub mod rvf;
pub mod udp;

pub use error::ProtoError;
