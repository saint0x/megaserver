# Megaserver

Megaserver is a self-hosted single-node PaaS runtime that turns one machine into a programmable application platform. It is built as a Rust runtime and daemon with a Fzy-owned control plane, planner surface, and ABI layer.

This tree is intentionally self-contained. Sibling projects were used as source material during implementation, but the runtime logic that Megaserver depends on now lives in this repository.

## Current Status

- Production-shaped single-node control plane, daemon API, and ingress are implemented and verified on macOS/host-process mode.
- The remaining substantive work is Linux substrate completion: real namespaces, cgroups, bridge networking, veth pairs, private DNS, firewalling, and network namespaces.
- `SPEC.md` is annotated with green checks only for functionality that is implemented and verified here.

## What Works

- Service lifecycle: `deploy`, `start`, `stop`, `restart`, `destroy`
- Control plane state and registry backed by SQLite
- Routes and expose flows
- Volumes, secrets, snapshots, and rollback
- Logs, shell, inspect, health, and events
- Daemon API routed through the Fzy HTTP normalization path
- Ingress reverse proxy with host-based routing
- TLS on both daemon API and ingress
- Signed ingress links
- WebSocket proxying
- Persisted sandbox metadata and runtime identity
- Fzy planner and control-plane interop over a stable C ABI

## Architecture

- `src/`
  - Fzy control plane, request normalization, planner exports, route catalog, and ABI wrappers
- `rust/src/`
  - Rust runtime, daemon, ingress proxy, TLS, state, runtime supervision, storage, and host ABI
- `tests/`
  - Deterministic Fozzy scenarios
- `artifacts/`
  - Recorded Fozzy traces used to verify real runs and replayability
- `SPEC.md`
  - Product spec with verified items marked
- `LINUXHANDOFF.md`
  - Detailed handoff for finishing the Linux-only substrate

## Quick Start

```bash
cd /Users/deepsaint/Desktop/megaserver
fz build --lib
cd rust
cargo run -- --home ../tmp/megahome init
cargo run -- --home ../tmp/megahome deploy ../examples/hello-service
cargo run -- --home ../tmp/megahome services
```

Run the daemon:

```bash
cd /Users/deepsaint/Desktop/megaserver/rust
cargo run -- daemon --home ../tmp/megahome --api 127.0.0.1:9000 --ingress 127.0.0.1:8089
```

## Verification

The repository has been verified with both Rust tests and Fozzy-first runtime checks.

Core checks:

```bash
cd /Users/deepsaint/Desktop/megaserver
fz build --lib
cargo test -p megaserver
fozzy test --det --strict \
  tests/megaserver.lifecycle.pass.fozzy.json \
  tests/megaserver.controlplane.pass.fozzy.json \
  tests/megaserver.daemon.pass.fozzy.json \
  tests/megaserver.daemon.tls.pass.fozzy.json \
  --json
```

Host-backed checks:

```bash
fozzy run tests/megaserver.daemon.pass.fozzy.json \
  --proc-backend host --fs-backend host --http-backend host --json

fozzy run tests/megaserver.daemon.tls.pass.fozzy.json \
  --proc-backend host --fs-backend host --http-backend host --json
```

Recorded traces:

- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.lifecycle.trace.production.fozzy`
- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.daemon.trace.production.fozzy`
- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.daemon.tls.trace.production.fozzy`
- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.daemon.signed.trace.production.fozzy`
- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.daemon.tls.signed.trace.production.fozzy`

## Linux Completion

Megaserver is intentionally not pretending the Linux substrate is finished on macOS. The current runtime reports honest host-process fallback metadata here while keeping the Linux hooks staged in the Rust runtime. The full handoff is in `/Users/deepsaint/Desktop/megaserver/LINUXHANDOFF.md`.
