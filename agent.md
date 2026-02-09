# Aegis Edge Relay Architecture

## Objective
Aegis Edge Relay provides a production-oriented TCP-over-WebSocket transport path for Network Resilience and Traffic Normalization across unstable routes, using Cloudflare edge ingress and Rust-based relay nodes.

## End-to-End Data Flow
1. `Local TCP Client` connects to the Bridge listener (`aegis-edge-relay bridge`).
2. `Rust Bridge` encapsulates byte streams into binary WebSocket frames.
3. `Cloudflare Anycast IP` receives TLS 1.3 traffic with configured `SNI` and `Host`.
4. `Cloudflare Worker` validates `Auth-Secret-Key`, upgrades WebSocket at ingress, and relays payload through `cloudflare:sockets connect()` to Exit.
5. `Rust Exit` (`aegis-edge-relay destination`) authenticates upstream and decapsulates frames back into TCP.
6. `Final Target (3x-ui)` receives native TCP on the configured local forward port.

## Transport Profile
- Protocol stack: `TCP -> TLS 1.3 -> WebSocket -> Binary Frames`.
- UDP is not used anywhere in the data path.
- Bridge client uses ALPN `http/1.1` for WebSocket upgrade compatibility.
- Worker upstream transport can run with plaintext TCP (`ws`) or TLS (`wss`) toward Exit.

## Traffic Normalization Profile
- Bridge supports explicit TLS profile labels (`chrome`, `firefox`) for stable, deterministic handshake behavior.
- SNI and Host are independently configurable to align ingress behavior with the deployed domain.
- Custom headers are kept small and consistent to reduce variance across sessions.

Note:
- This implementation uses standards-compliant TLS via `rustls`.
- If strict browser-level fingerprint parity is required, place a controlled TLS terminator in front of the Bridge that supports that profile model.

## Active Session Cycling
Bridge sessions rotate based on either threshold:
- Time window: default 10 minutes (configurable; operational range 10-15 minutes).
- Data window: default 100 MB (`104857600` bytes).

When either threshold is reached, new incoming streams are assigned to a new generation while existing streams continue on the previous generation until completion. This enables graceful migration without interrupting active flows.

## Authentication and Security
- Shared secret header: `Auth-Secret-Key`.
- Validation points:
  - Worker: verifies inbound secret before proxying.
  - Exit node: verifies `AUTH <secret>` preface on raw socket mode and keeps WebSocket-header verification for compatibility mode.
- Secrets are intended to be injected via environment (`AUTH_SECRET_KEY`) in production service units.
- TLS 1.3 is enforced at bridge ingress to Cloudflare.

## Components
- `worker.js`: Cloudflare Worker reverse proxy with WebSocket upgrade support.
- `src/main.rs`: Rust relay binary with `bridge` and `destination` modes.
- `deploy.sh`: installer + build + TUI configuration + systemd provisioning.

## Operational Ports
- Bridge listen port: user-specified local TCP entry.
- Edge port: typically `443`.
- Exit WebSocket listen port: default `8443`.
- Final target port: user-specified local service port (for example `8080`).

## Failure Handling
- Connection attempts are per-session and isolated; failure does not block listener accept loops.
- `systemd` restart policy keeps service continuity (`Restart=always`).
- TCP `nodelay` is enabled on relay sockets to reduce queuing latency.
