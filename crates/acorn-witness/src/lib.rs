//! `acorn-witness` — tamper-evident audit trail + device attestation.
//!
//! Phase 1: hash-linked log H(prev || record); Ed25519 device keypair
//!          generated on first boot; attestation = sign{epoch, count, head}.
