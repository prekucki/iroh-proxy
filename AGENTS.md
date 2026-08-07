# AGENTS.md

This file guides coding agents and contributors working in this repository.

## Project goal

Build a small, reliable Rust CLI that uses iroh to proxy services over TCP.

Primary workflow:

1. `server` on the service host (typically user systemd).
2. `add-serve [-p] <name> <target>` on the service host.
3. `forward <listen> <endpoint-id>/tcp/<name>` on the client host.

## Scope

- Keep the project focused on transport/proxy behavior.
- Prefer simple, debuggable implementations over premature abstractions.
- Add protocol-specific logic only when explicitly requested.

## Tech stack

- Rust stable
- Tokio async runtime
- iroh networking
- clap for CLI parsing
- anyhow for error handling

## Code guidelines

- Keep modules small and explicit.
- Use clear error messages with context (`with_context`).
- Prefer straightforward async tasks and explicit connection lifecycle handling.
- Keep CLI UX stable; avoid breaking command shapes without updating README.

## Networking behavior requirements

- Only accept remote paths shaped as `<endpoint-id>/tcp/<service>`.
- ALPN must be deterministic per service name.
- Keep bidirectional forwarding as two one-way copies.
- Use persistent key material so endpoint id remains stable across restarts.

## Safety and operational constraints

- Do not add destructive behavior.
- Avoid hidden background processes in tests.
- Do not log secrets or key bytes.

## Before submitting changes

Run:

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo check
```

If behavior changes, update:

- `README.md` usage examples
- this `AGENTS.md` when conventions shift

## Future extension directions

When requested, prefer incremental additions in this order:

1. Access controls (allowlist/token) on `server`.
2. UDP or protocol-aware modes (HTTP/WebSocket) as separate commands.

Already implemented:

- Multi-service mapping in one `server` process.
- Selective connection retry/backoff in `forward`, bounded by a total deadline
  (`proxy::connect_remote_with_retry`, applied to all forward modes).
