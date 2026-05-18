//! `acornd` — wire-compatible Cognitum Seed daemon.
//!
//! Wiring (fill in per phase):
//!   ingest        (UDP  :5006)  -> acorn-store
//!   acorn-api     (HTTPS :8443) -> acorn-store / acorn-witness / acorn-cognitive
//!   acorn-mcp     (JSON-RPC)    -> same handler set
//!   acorn-sensors (GPIO/I2C)    -> acorn-store fusion

fn main() {
    println!("acornd 0.1.0 - scaffold. Not yet implemented; see README.md phasing.");
}
