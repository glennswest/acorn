//! `acorn-api` — HTTPS server (:8443), the RuView-compatible endpoint surface.
//!
//! Phase 1: /pair, /pair/window (USB-iface only), /store/{ingest,query,
//!          export,compact}, /witness/verify, /custody/attestation,
//!          /system/health. Bearer token stored as SHA-256 hash.
//! Later:   /boundary, /coherence/*, /cognitive/*, /sensor/*, /reflex/*,
//!          /swarm/* — fill out to the full 98.
