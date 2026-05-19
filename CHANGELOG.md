# Changelog

## [v0.2.0] — 2026-05-19

### Added
- **Phase 1 wire-compatible core**
  - `acorn-witness`: SHA-256 hash-linked chain (`H(prev || record)`) with
    persisted append log + Ed25519 device custody (key 0600 on unix). Signed
    attestations over `(epoch, count, head)`.
  - `acorn-store`: RVF append-only store with 32-byte header (4 + 14 +
    14 reserved) + 42-byte records. Brute-force kNN over cosine / L2 / dot.
  - `acorn-api`: Axum router with `/pair`, `/pair/window`, `/store/*`,
    `/witness/verify`, `/custody/attestation`, `/system/health`. Bearer auth
    using constant-time SHA-256 comparison against the hash issued by `/pair`.
  - `acornd`: clap CLI + tokio runtime; spawns Axum on `--http-addr` and UDP
    ingest on `--udp-addr` (parses `EdgeFeaturePkt`, content-addresses ids
    via truncated SHA-256, appends to chain+store).
  - `acorn-client`: typed reqwest async client.
- **Phase 2 sensors + reflex**
  - `Sensor` trait (async) with mock implementations (`MockDigital`,
    `MockAdc`, `MockClimate`).
  - Real Pi drivers (rppal/ads1x1x/bme280) gated behind `pi-hw` feature;
    wiring stubs in place.
  - `Reflex` state machine emitting `SensingEvent` transitions only: Fall on
    rising edge, Occupancy on threshold crossing, Vitals when occupied + in
    plausible range, Motion on rising edge.
- **Phase 3 cognitive**
  - `Cognitive::snapshot` builds a kNN similarity graph (weight =
    `1/(1+distance)`), runs Stoer-Wagner global min-cut, produces fragility +
    coherence.
  - `/api/v1/boundary`, `/api/v1/coherence`, `/api/v1/cognitive/snapshot`
    endpoints + matching client methods.
  - Scoring formulas (`fragility = 1/(1+min_cut)`, `coherence = 1/(1+stddev)`)
    documented as Acorn-defined.
- **Phase 4 MCP + swarm**
  - JSON-RPC 2.0 dispatcher: `JsonRpcRequest`/`Response`/`Error` types, tool
    registry, built-in `mcp.list_tools` introspection.
  - 9 representative `seed.*` tools across `memory`/`witness`/`cognitive`/`sensor`
    namespaces — adding more is mechanical.
  - `POST /api/v1/mcp` HTTP transport, bearer-auth.
  - Swarm peer registry: `/swarm/peers` (list/add/remove) +
    `/swarm/sync` (epoch/head exchange, reports ahead/behind/synced).

### Documentation
- README reflects current state, build/run instructions, TLS note, sensor
  feature flag, and caveats (RVF provisional, swarm pull TODO, etc.).

## [0.1.0] — 2026-05-18

### Added
- Initial workspace scaffold, renamed from "seed" → "acorn" on the same day.
- `acorn-proto` implemented (wire types, RVF format, HTTP DTOs, sensing event
  vocabulary). All other crates as documented stubs with per-phase TODOs.

## [Unreleased]
<!-- New unreleased changes go here -->
