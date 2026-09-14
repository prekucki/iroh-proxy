# Relay recovery dependency

The root `[patch.crates-io]` table selects a reviewed commit from
[prekucki/iroh](https://github.com/prekucki/iroh), based on the source commit used
to publish iroh 1.0.3. Dependency sources and regression tests live in that fork,
not in this repository. See the fork's
[patch notes](https://github.com/prekucki/iroh/blob/b61fa39a76e0f22d2db21c9eb2f77e6df7a8de0d/RELAY_RECOVERY.md).

`iroh`, `iroh-base`, `iroh-dns`, and `iroh-relay` must use the same revision.
The git workspace has path dependencies between these crates. Patching all
four prevents duplicate crate identities when the proxy's address-lookup
dependencies also use them. Versions of other runtime dependencies stay locked.

## Recovery behavior

- Relay close handshakes have a 3-second deadline and can be cancelled.
- Internal control enqueueing and route-query enqueueing/replies have a
  5-second deadline. Failed actors are aborted and reaped before replacement;
  the home relay is recreated automatically, other relays on demand.
- Actors continue answering control messages during reconnect backoff.
- Asynchronous interface snapshots and graceful child shutdown have a
  5-second deadline. Shutdown tolerates already-cancelled tasks and aborts
  remaining tasks if graceful shutdown expires.
- The proxy waits at most 20 seconds for endpoint relay readiness, then logs a
  warning and continues with the same endpoint and background reconnection.

The endpoint identity, server service mappings, and wire protocol are unchanged.
Established TCP streams are not replayed or resumed. Async timeouts cannot
interrupt synchronous code that never yields. The patch addresses unbounded
waits consistent with the incident logs; it does not prove their precise cause.

## Verification and upgrades

CI locates the exact iroh source selected by `cargo metadata`, checks the patched
files' formatting, and runs `relay_recovery` library tests on Linux, macOS, and
Windows. The tests use a local relay, including a datagram exchange after
replacing an unresponsive home relay. They require no public relay service.
See `AGENTS.md` for the local test command.

Update all four revision pins together and run the application checks plus
the fork's recovery tests. Remove the overrides when a reviewed upstream
release provides equivalent recovery behavior and passes those tests.
