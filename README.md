# acorn

A **wire-compatible reimplementation of the Cognitum Seed** edge-intelligence
appliance, in Rust. It speaks the RuView protocol (UDP feature packets + the
HTTPS `/api/v1/*` surface) so it drops in for the Pi Zero 2 W Seed, and is
designed so its sensing output can later be consumed by the Z Man home
automation platform without coupling the two codebases.

Decision (locked): **(a) full wire-compatible Seed clone** — RVF store, witness
chain, Ed25519 custody, the ~98-endpoint HTTP surface, MCP proxy, cognitive
layer. (Not the leaner "Z Man sensing service" option.)

Companion spec: `cognitum-seed-rust-scoping.md`.

## Workspace layout

| Crate | State | Role |
|---|---|---|
| `acorn-proto` | **implemented** | Wire types, RVF format, HTTP DTOs, sensing-event vocab. The integration boundary — only depends on `serde`. |
| `acorn-client` | stub | Async client for the Seed API. **Z Man imports this + `acorn-proto`** and nothing else. |
| `acorn-store` | stub | RVF append-only store, brute-force kNN, kNN graph. |
| `acorn-witness` | stub | SHA-256 hash-linked witness chain + Ed25519 device custody. |
| `acorn-sensors` | stub | Pi GPIO/I2C drivers (reed, PIR, vibration, ADS1115, BME280). |
| `acorn-cognitive` | stub | Stoer-Wagner min-cut fragility + temporal coherence. |
| `acorn-api` | stub | Axum HTTPS server — the RuView-facing endpoint surface. |
| `acorn-mcp` | stub | MCP proxy: ~114 tools over JSON-RPC 2.0. |
| `acornd` | stub | The daemon binary; wires the crates together. |

`acorn-proto` is real and tested; every other crate is a documented stub whose
`lib.rs` carries the per-phase TODO outline. Heavy dependencies are staged
(commented) in the workspace `Cargo.toml` — uncomment per crate as you build.

## Integration seam (Z Man)

Z Man consumes Acorn the way it consumes any other sensor source — over the
network, as an event source — and at the source level depends **only** on
`acorn-proto` (types) and `acorn-client` (transport). It never links
`acorn-store`, `acorn-api`, `acorn-sensors`, etc. The event vocabulary lives in
`acorn-proto::event` (`SensingEvent`: occupancy / motion / fall / vitals /
regime-change) so the mapping is defined once, up front.

## Phasing

1. **Wire-compatible core** — `acorn-store` + `acorn-witness` + a UDP ingest path
   + `acorn-api` with pair / store{ingest,query,export,compact} / witness/verify
   / custody/attestation / system/health. Target: RuView's
   `seed_csi_bridge.py --validate` passes against `acornd`.
2. **Sensors + reflex rules** — `acorn-sensors` + the 3 reflex rules.
3. **Cognitive layer** — `acorn-cognitive`: boundary / coherence / cognitive
   snapshot. Scoring semantics are underspecified upstream — define your own.
4. **MCP + swarm** — `acorn-mcp` (114 tools) + epoch-based swarm sync.

## Before Phase 1

The one provisional thing in `acorn-proto` is the **RVF binary layout**
(`rvf.rs`). Confirm the real header and record stride against a `hexdump -C` of
`GET /api/v1/store/export` (or the `wifi-densepose-train` parser) and adjust
`RvfRecord::WIRE_LEN` before building `acorn-store` on top of it.

## Build

```sh
cargo check --workspace
cargo test  -p acorn-proto
```
