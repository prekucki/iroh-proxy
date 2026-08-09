# Roadmap

This document describes the intended development order for `iroh-proxy`.
It is a direction, not a release-date commitment.

The project remains focused on being a small, reliable, and debuggable TCP
proxy over iroh. New protocols and application-level features require a
concrete use case before they enter the core CLI.

## Current baseline

Released in v0.2:

- Multiple service mappings handled by one server process.
- Persistent server identity and optional persistent client identities.
- Selective forward connection retry with exponential backoff and a bounded
  total deadline.
- TCP forwarding in listener, stdio, fd-pass, and daemon-managed modes.
- Live control and status support on Linux, macOS, and Windows.
- TUI management for services, forwards, and active connections.
- User systemd integration on Linux.

The following invariants remain in force:

- TCP remote paths use `<endpoint-id>/tcp/<service>`.
- ALPN values are deterministic per service name.
- Forwarding remains two explicit one-way copies with well-defined half-close
  and teardown behavior.
- Established streams are never transparently replayed or resumed after a
  disconnect.
- Persistent key material and other secrets are never logged.

## P0: bounded endpoint startup

Prevent relay readiness from blocking startup indefinitely.

Planned work:

- Bound the wait for `Endpoint::online()`.
- Continue in an explicit degraded state when relay readiness is unavailable.
- Allow viable direct and local discovery paths to proceed without waiting
  forever for a relay connection.
- Report whether startup is online, degraded, or failed, with an actionable
  reason.
- Make endpoint-readiness and remote-connect deadlines clear in CLI output and
  documentation.

Completion criteria:

- Server and forward commands cannot wait indefinitely for endpoint readiness.
- Relay unavailability produces one clear warning instead of a silent hang.
- A degraded endpoint can still attempt viable direct or local connections.
- Timeout and degraded-start behavior have deterministic tests.
- Existing successful startup behavior and CLI shapes remain compatible.

## v0.3: access control and resource safety

### EndpointId allowlists

Add server-side authorization based on the authenticated remote iroh
`EndpointId`.

Planned work:

- Support allowlists per served service.
- Reject unauthorized peers before connecting to the local target.
- Define explicit configuration semantics for public access, allowlists, and
  deny-all.
- Preserve current open behavior for existing configurations while making it
  visible to the operator.
- Define whether policy changes terminate existing tunnels or affect only new
  connections.
- Log authorization decisions without logging private keys or other secrets.

### Identity UX

Make allowlists practical to configure and troubleshoot.

Planned work:

- Provide a clear way to display the local public `EndpointId`.
- Document how clients create and reuse persistent identities.
- Show which identity a forward operation uses.
- Provide actionable diagnostics for rejected or incorrectly configured peers.

### Resource limits

Bound work accepted from remote peers.

Planned work:

- Add explicit deadlines for connection setup, first stream acceptance, and
  local-target connection.
- Add configurable concurrent-connection limits globally, per service, and,
  where useful, per peer.
- Cancel pending setup work promptly when a service is removed or the server
  shuts down.
- Reject excess work predictably and release resources promptly.
- Expose concrete timeout and limit reasons in logs and status output.

Completion criteria:

- An unauthorized peer never reaches the configured local target.
- Allow, reject, deny-all, and backward-compatible public configurations are
  covered by tests.
- A documented workflow produces a stable client `EndpointId`.
- Idle or incomplete connections are reclaimed within configured bounds.
- Connection limits and policy changes are tested and observable to the
  operator.

## v0.4: operability

Improve deployment and diagnosis without expanding protocol scope.

Planned work:

- Add stable machine-readable status output.
- Report endpoint readiness and degraded or offline state.
- Report active connections and accepted, rejected, failed, and retried
  connection counts.
- Preserve the last actionable service or connectivity error.
- Distinguish direct and relayed paths where the stable iroh API exposes that
  information.
- Improve configuration validation and startup diagnostics.
- Evaluate native user-service installation for macOS and Windows, keeping
  system-specific implementations separate.

Completion criteria:

- Operators can distinguish connectivity, authorization, resource-limit, and
  local-target failures without enabling debug instrumentation.
- Machine-readable output is documented and covered by compatibility tests.
- Service lifecycle tasks shut down cleanly without hidden background
  processes.

## Later, only with a concrete requirement

These items are intentionally not scheduled.

### UDP

UDP must be a separate command or mode. Its design must first define session
mapping, idle expiry, datagram size and fragmentation behavior, limits, access
control, and failure reporting.

### Protocol-aware HTTP or WebSocket modes

HTTP and WebSocket already work through raw TCP forwarding. Protocol-aware
support should be added only for a specific requirement such as host or path
routing, application authentication, or TLS termination.

### Token authentication

EndpointId allowlists are preferred first. Token authentication should be
considered only when delegated or non-key-based access is required. It needs a
versioned handshake plus explicit secret storage, rotation, and redaction
rules. Tokens must not be encoded in ALPN values or logs.

### Connection reuse

QUIC connection pooling or multiplexing should follow measurements that show a
meaningful need. Its design must cover reconnect behavior and failure fan-out
without making connection lifecycle debugging opaque.

## Out of scope for the core CLI

Filesystem share and FUSE mount support should not be merged into the core
proxy. If revived, it should live in a separate crate, binary, or repository
with its own protocol versioning, platform support policy, and security model.

## Definition of done

Every roadmap item that changes behavior must include:

- Focused unit and integration tests.
- `cargo fmt`.
- `cargo clippy --all-targets --all-features -- -D warnings`.
- `cargo check`.
- Updated README examples and operational diagnostics.
- Cross-platform validation where the affected code is not platform-specific.
- Network-topology coverage appropriate to the change, including direct,
  relayed, degraded, or offline scenarios where relevant.
- No secret or private key material in logs.
