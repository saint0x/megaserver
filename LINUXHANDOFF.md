# Linux Handoff

This document is the production handoff for finishing Megaserver on Linux. It is meant to let the next engineer start immediately without reconstructing context from chat history.

## Executive Summary

Megaserver is already in a strong state for the non-Linux stack:

- Fzy owns control-plane normalization, planner exports, route modeling, and the stable ABI layer.
- Rust owns the runtime, daemon, ingress, TLS, state, storage, and host ABI implementation.
- The CLI, daemon API, ingress proxy, TLS, signed links, WebSocket proxying, volumes, secrets, snapshots, rollback, inspect, shell, logs, routes, and events are implemented and verified.
- The remaining meaningful work is the Linux substrate:
  - true namespaces
  - true cgroups
  - bridge networking
  - veth pairs
  - private DNS
  - firewalling
  - network namespaces
  - real service-to-service private networking

In short: the control plane is real, the app-plane plumbing is real, and the missing work is now primarily Linux kernel/runtime plumbing.

## What Is Done

### Control Plane

- Fzy planner exports compile and link through `fz build --lib`.
- Fzy HTTP normalization handles daemon request envelopes and maps them into canonical host-dispatch actions.
- Route metadata is modeled in Fzy and exported as a single source of truth.
- Control-plane requests preserve `home`, JSON bodies, and action semantics correctly.
- Nested `Fzy -> Rust host -> Fzy planner` deploy flow is working and regression-tested.

Key files:

- `/Users/deepsaint/Desktop/megaserver/src/main.fzy`
- `/Users/deepsaint/Desktop/megaserver/src/api/ffi.fzy`
- `/Users/deepsaint/Desktop/megaserver/src/model/contracts.fzy`
- `/Users/deepsaint/Desktop/megaserver/src/model/control.fzy`
- `/Users/deepsaint/Desktop/megaserver/src/model/request.fzy`
- `/Users/deepsaint/Desktop/megaserver/src/model/response.fzy`
- `/Users/deepsaint/Desktop/megaserver/src/services/control.fzy`
- `/Users/deepsaint/Desktop/megaserver/src/services/http.fzy`
- `/Users/deepsaint/Desktop/megaserver/src/services/web.fzy`

### Rust Runtime And Daemon

- SQLite-backed state plane is in place.
- Service lifecycle commands are in place.
- Persisted sandbox records exist with runtime metadata and identity.
- Daemon API is implemented and routed through Fzy normalization instead of duplicating route/action logic in Rust.
- Ingress proxy is implemented.
- TLS is implemented for both the daemon API and ingress.
- Signed ingress links are implemented and validated with HMAC + expiry.
- WebSocket proxying is implemented and covered by Rust integration tests.

Key files:

- `/Users/deepsaint/Desktop/megaserver/rust/src/app.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/cli.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/controlplane.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/daemon.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/ffi.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/host_abi.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/ingress.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/proxy.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/runtime.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/sandbox.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/state.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/tls.rs`

### Storage / Ops Surfaces

- Volumes
- Secrets
- Snapshots
- Rollback
- Inspect
- Events
- Logs
- Shell

These are already wired through the live runtime/control-plane path and are not blocked on Linux-specific behavior except where stronger sandbox/network isolation would improve the production posture.

## What Is Verified

### Rust Test Coverage

Passing `cargo test -p megaserver` covers:

- init/home bootstrap
- Fzy control-plane reentry without deadlock
- HTTP control dispatch preserving `home` and JSON body semantics
- daemon API + ingress end-to-end
- daemon HTTPS for API + ingress
- signed-link round-trip
- WebSocket proxy path
- sandbox resource parsing

### Fozzy Coverage

Strict deterministic scenarios:

- `/Users/deepsaint/Desktop/megaserver/tests/megaserver.lifecycle.pass.fozzy.json`
- `/Users/deepsaint/Desktop/megaserver/tests/megaserver.controlplane.pass.fozzy.json`
- `/Users/deepsaint/Desktop/megaserver/tests/megaserver.daemon.pass.fozzy.json`
- `/Users/deepsaint/Desktop/megaserver/tests/megaserver.daemon.tls.pass.fozzy.json`

Representative commands already run successfully:

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

Host-backed runs already passed:

```bash
fozzy run tests/megaserver.daemon.pass.fozzy.json \
  --proc-backend host --fs-backend host --http-backend host --json

fozzy run tests/megaserver.daemon.tls.pass.fozzy.json \
  --proc-backend host --fs-backend host --http-backend host --json
```

Recorded traces already verified and replayed:

- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.lifecycle.trace.production.fozzy`
- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.daemon.trace.production.fozzy`
- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.daemon.tls.trace.production.fozzy`
- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.daemon.signed.trace.production.fozzy`
- `/Users/deepsaint/Desktop/megaserver/artifacts/megaserver.daemon.tls.signed.trace.production.fozzy`

### Fzy Build State

`fz build --lib` is working and produces the library artifacts Megaserver expects.

There is still an informational Fzy warning during build about explicit unsafe escape markers in `src/main.fzy`:

- `W-VER-C53E0DE5`

That warning is not currently blocking the verified path, but it is worth keeping visible during Linux hardening because the Linux substrate work will naturally touch the unsafe boundary again.

## What Is Still Blocked By Linux

These are the remaining meaningful gaps.

### 1. True Sandbox Isolation

The code already stages sandbox runtime metadata and Linux-specific hooks, but the live verified macOS path honestly falls back to host-process supervision.

What still needs Linux:

- UTS namespace setup
- mount namespace setup
- IPC namespace setup
- PID namespace strategy if desired
- real cgroup hierarchy creation and process attachment
- stronger resource enforcement beyond metadata and soft limits

Primary files:

- `/Users/deepsaint/Desktop/megaserver/rust/src/sandbox.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/runtime.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/app.rs`

### 2. Private Networking

Current state:

- Megaserver has deterministic sandbox identity metadata, including hostname and a deterministic private IP value, and it persists this in state.
- That identity is real at the control-plane/runtime metadata level, but not yet backed by Linux network namespaces and bridge plumbing.

Still needed:

- Linux bridge device setup
- veth pair creation
- interface wiring per sandbox
- route configuration inside sandbox namespaces
- private service-to-service connectivity
- DNS registration / resolution for internal names
- firewalling / network policy posture

Likely touchpoints:

- `/Users/deepsaint/Desktop/megaserver/rust/src/runtime.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/sandbox.rs`
- `/Users/deepsaint/Desktop/megaserver/rust/src/state.rs`
- any Linux-only networking module you create or expand around the existing runtime plane

### 3. Linux Validation Matrix

The runtime needs a Linux verification pass for:

- service startup inside the real sandbox path
- cgroup attachment and cleanup
- namespace cleanup on stop/destroy
- private-network reachability between services
- ingress routing from host into private service network
- snapshots / rollback with active isolated workloads

## Suggested Order Of Work

1. Keep the Fzy control plane unchanged unless Linux work truly requires ABI additions.
2. Finish Linux sandbox setup in `/Users/deepsaint/Desktop/megaserver/rust/src/sandbox.rs`.
3. Wire real Linux runtime launch behavior through `/Users/deepsaint/Desktop/megaserver/rust/src/runtime.rs`.
4. Add the bridge / veth / private-network plane.
5. Attach persisted sandbox state to the real Linux substrate objects.
6. Add Linux-backed Fozzy scenarios or host-backed Linux CI flows that exercise the true isolated path.
7. Only after that, mark the remaining unchecked `SPEC.md` items.

## Practical Notes

- The control plane is already in a good place. Do not re-centralize route normalization back into Rust; the daemon/API path now intentionally flows through Fzy.
- Signed links, TLS, ingress, and WebSockets are already implemented. The next engineer should not spend time rebuilding those.
- Sandbox identity fields already exist and are threaded through runtime env:
  - `MEGASERVER_SANDBOX_ID`
  - `MEGASERVER_SANDBOX_HOSTNAME`
  - `MEGASERVER_SANDBOX_IP`
  - `HOSTNAME`
- `SPEC.md` has been updated to show what is already done and verified. Use it as the truth source for status tracking.

## First Commands To Run On Linux

```bash
cd /path/to/megaserver
fz build --lib
cargo test -p megaserver
fozzy test --det --strict \
  tests/megaserver.lifecycle.pass.fozzy.json \
  tests/megaserver.controlplane.pass.fozzy.json \
  tests/megaserver.daemon.pass.fozzy.json \
  tests/megaserver.daemon.tls.pass.fozzy.json \
  --json
```

Then add Linux-specific end-to-end checks around:

- sandbox creation
- namespace membership
- cgroup placement
- bridge/veth wiring
- internal DNS
- ingress-to-private-service flow

## Bottom Line

Megaserver is no longer blocked on ordinary platform plumbing. The next engineer should treat this as a Linux substrate completion task, not as a control-plane or app-runtime greenfield effort.
