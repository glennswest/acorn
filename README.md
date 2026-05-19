# acorn

A **wire-compatible reimplementation of the Cognitum Seed** edge-intelligence
appliance, in Rust. It speaks the RuView protocol (UDP feature packets + the
HTTPS `/api/v1/*` surface) so it drops in for the Pi Zero 2 W Seed, and is
designed so its sensing output can later be consumed by the Z Man home
automation platform without coupling the two codebases.

Decision (locked): **(a) full wire-compatible Seed clone** — RVF store, witness
chain, Ed25519 custody, the HTTP surface, MCP proxy, cognitive layer. (Not the
leaner "Z Man sensing service" option.)

Companion spec: `cognitum-seed-rust-scoping.md` (external).

## Workspace layout

| Crate | State | Role |
|---|---|---|
| `acorn-proto` | implemented | Wire types, RVF format, HTTP DTOs, sensing-event vocab. The integration boundary — only depends on `serde`. |
| `acorn-client` | implemented | Typed async reqwest client over the Acorn HTTP surface. **Z Man imports this + `acorn-proto`** and nothing else. |
| `acorn-store` | implemented | RVF append-only store, brute-force kNN over cosine/L2/dot. |
| `acorn-witness` | implemented | SHA-256 hash-linked witness chain + Ed25519 device custody. |
| `acorn-sensors` | implemented | Sensor trait + mocks + Reflex rules. Pi-hardware drivers gated behind `pi-hw` feature (rppal/ads1x1x/bme280 wiring TODO). |
| `acorn-cognitive` | implemented | Stoer-Wagner min-cut fragility + temporal coherence. |
| `acorn-api` | implemented | Axum HTTP server — pair/store/witness/custody/cognitive/mcp/swarm endpoints (12 routes; 98-endpoint full surface follows the same pattern). |
| `acorn-mcp` | implemented | JSON-RPC 2.0 dispatcher + tool registry. Ships ~9 representative `seed.*` tools spanning all 4 namespaces; adding more is mechanical. |
| `acornd` | implemented | Daemon binary; wires UDP ingest + HTTP API + everything else. |

## Integration seam (Z Man)

Z Man consumes Acorn the way it consumes any other sensor source — over the
network, as an event source — and at the source level depends **only** on
`acorn-proto` (types) and `acorn-client` (transport). It never links
`acorn-store`, `acorn-api`, `acorn-sensors`, etc. The event vocabulary lives in
`acorn-proto::event` (`SensingEvent`: occupancy / motion / fall / vitals /
regime-change) so the mapping is defined once, up front.

## Build & test

```sh
cargo build --workspace
cargo test  --workspace
```

The full test suite is 36 unit + integration tests across the workspace.

## Run

```sh
acornd \
  --http-addr 0.0.0.0:8443 \
  --udp-addr 0.0.0.0:5006 \
  --store    acorn-store.rvf \
  --witness  acorn-witness.log \
  --custody  acorn-custody.key \
  --metric   cosine          # cosine | l2 | dot
```

All flags also read from env vars (`ACORND_HTTP_ADDR`, `ACORND_STORE`, …).

Pair, ingest, query, attest:

```sh
TOKEN=$(curl -s -X POST http://127.0.0.1:8443/api/v1/pair \
  -H 'content-type: application/json' \
  -d '{"client_name":"hello"}' | jq -r .token)

curl -s http://127.0.0.1:8443/api/v1/system/health
curl -s -X POST http://127.0.0.1:8443/api/v1/store/ingest \
  -H "authorization: Bearer $TOKEN" -H "content-type: application/json" \
  -d '{"vectors":[[1,[1,0,0,0,0,0,0,0]]],"metadata":{"node_id":1,"type":"csi_feature","timestamp":0}}'

curl -s -X POST http://127.0.0.1:8443/api/v1/mcp \
  -H "authorization: Bearer $TOKEN" -H "content-type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"mcp.list_tools"}'
```

## TLS

Out of scope for the daemon itself in this release — terminate TLS in front
(caddy / nginx / traefik) on the same host. A built-in `rustls` listener is a
clean follow-up.

## Sensor hardware

Real Pi drivers are gated behind the `pi-hw` cargo feature on `acorn-sensors`
and require an actual Pi to validate. The default build is host-portable and
uses deterministic mocks (`MockDigital`, `MockAdc`, `MockClimate`).

## Caveats

* **RVF binary layout is provisional.** Reconstructed from ADR-069's
  storage-budget arithmetic (~40 bytes/record). Confirm against a `hexdump -C`
  of a real `GET /api/v1/store/export` (or the `wifi-densepose-train` parser)
  and adjust `RvfRecord::WIRE_LEN` if the real stride differs.
* **Cognitive scoring semantics are Acorn-defined.** Upstream is silent on the
  formula. We use `fragility = 1/(1+min_cut)` and `coherence = 1/(1+stddev)`
  over the recent window. Both are bounded in `(0, 1]`.
* **Witness reconciliation across swarm peers is a follow-up.** The Phase 4
  swarm endpoints expose peer registration and an epoch/head exchange; they
  log "behind" rather than pulling deltas, because cross-chain merge is
  non-trivial under the integrity invariants.
* **Long-tail endpoints and MCP tools.** The Phase 1-4 surface is implemented
  along the same handler pattern as the rest of the spec — extending to the
  full 98 endpoints / 114 tools is a mechanical exercise from here.
