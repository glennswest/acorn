//! `acorn-client` — async client for the Seed HTTP / MCP / push API.
//!
//! This crate plus `acorn-proto` is the *only* thing Z Man depends on.
//! Keep it free of appliance-internal crates (no acorn-store / acorn-api / etc.).
//!
//! Phase 1: `AcornClient { pair, ingest, query, verify_witness, attestation }`
//! Phase 4: `subscribe() -> Stream<acorn_proto::event::SensingEvent>`
