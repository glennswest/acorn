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

## [v0.3.0] — 2026-05-20

### Added
- **Live event pipeline.**
  - `EventBus` in acorn-api: tokio broadcast channels for `SensingEvent`
    and `RawReadingEvent`, plus an in-memory webhook URL registry.
  - `GET /api/v1/events` — Server-Sent Events stream of `SensingEvent`s
    (newline-delimited JSON, 15s keep-alive pings).
  - `GET /api/v1/ws` — WebSocket upgrade serving the same JSON.
  - `GET / POST / DELETE /api/v1/webhooks` — register fire-and-forget
    HTTP POST destinations for sensing events.
  - Webhook fan-out background task with 5s per-request timeout.
- **UDP ingest → Reflex → broadcast** wired in `acornd::ingest`. Each
  feature packet is now observed in the node registry, persisted to
  store+witness, evaluated by `Reflex`, and any resulting `SensingEvent`s
  are published on the bus.
- **ESP32 fleet observability.** `NodeRegistry` tracks per-node
  last-seen, packet count, last sequence, and detects sequence gaps
  (sub-16-bit modular). New endpoints:
  - `GET /api/v1/nodes` — all known nodes
  - `GET /api/v1/nodes/:id` — one node
- **Sensor poll task** in `acornd`. Polls every configured sensor at
  `--sensor-poll-ms` cadence, publishes `RawReadingEvent` to the bus.
  Bound list built from CLI flags; default uses mocks, `--features pi-hw`
  switches to real drivers (with graceful fall-back if a constructor
  errors).
- **Real Pi hardware drivers** under `pi-hw` feature on `acorn-sensors`:
  rppal-backed GPIO inputs (pull-up), `ads1x1x` ADS1115 (one-shot,
  ±4.096V FSR, 4 channels), `bme280` climate. All compile gated on
  `target_os = "linux"`; off-Pi builds get `HardwareUnavailable`.
- **`acornd` CLI flags** for the new surface:
  `--reed-pin`, `--pir-pin`, `--vibration-pin` (BCM numbers, default 5/6/13),
  `--i2c-bus` (default `/dev/i2c-1`), `--ads1115-addr`, `--bme280-addr`
  (parsed as `0x..` or decimal), `--sensor-poll-ms`, `--sensors-off`,
  `--presence-threshold`, `--motion-threshold`. All also via `ACORND_*`
  env vars.

### Verified end-to-end
- UDP packet with `presence=0.9` from `node_id=42` arrives → node appears
  in `/api/v1/nodes` with packet_count=1.
- Second packet with `seq=4` (skipping 2,3) → `gaps=2`.
- Third packet flips presence to 0.0 → `SensingEvent::Occupancy
  {occupied:false}` arrives on the SSE stream `data:` line.
- Webhook add/list returns assigned id and the registered URL.

## [Unreleased]
<!-- New unreleased changes go here -->
