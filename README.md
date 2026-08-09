# iroh-proxy

`iroh-proxy` is a small Rust CLI that forwards local TCP services over [iroh](https://github.com/n0-computer/iroh).

It lets you expose a service on one machine and access it locally on another machine through an iroh connection.

## Status

Early prototype intended for simple TCP forwarding workflows.

## Install / Build

```bash
cargo build --release
```

Binary path:

```bash
./target/release/iroh-proxy
```

## Commands

```text
iroh-proxy [--key-file <path>] [--config-file <path>] <command>
```

### `server`

Run the long-lived proxy server that exposes a platform-native control API.

`server` loads initial served routes from config (`[serve]`) and persisted forward listeners (`[forward]`).

```bash
iroh-proxy server
```

The server shuts down gracefully on `SIGTERM`/`SIGINT` (Ctrl-C on Windows) and
removes its control socket on exit (macOS/Windows). If the underlying iroh
endpoint closes, the process exits with a non-zero status so a supervisor
(systemd, launchd) can restart it instead of leaving a live-but-dead process.

Install a user systemd unit:

```bash
iroh-proxy server --install
```

This writes:

```text
~/.config/systemd/user/iroh-proxy.service
```

Then enable it:

```bash
systemctl --user daemon-reload
systemctl --user enable --now iroh-proxy.service
```

### Diagnostics

Logs are written to stderr (and therefore to the journal under systemd). Stdout
is kept exclusively for command output and the `forward` stdio transport.

Follow server logs:

```bash
journalctl --user -u iroh-proxy.service -f -o cat
```

For a focused diagnostic run in the foreground:

```bash
RUST_LOG='warn,iroh_proxy=debug,iroh::_events=debug,iroh::socket::transports::relay::actor=debug,noq_proto::connection::paths=debug' \
  iroh-proxy server
```

For an installed user service, create a persistent override with
`systemctl --user edit iroh-proxy.service`:

```ini
[Service]
Environment="RUST_LOG=warn,iroh_proxy=debug,iroh::_events=debug,iroh::socket::transports::relay::actor=debug,noq_proto::connection::paths=debug"
```

Then apply it:

```bash
systemctl --user daemon-reload
systemctl --user restart iroh-proxy.service
```

Incoming connection logs identify the lifecycle `stage` (`handshake`, `route`,
`accept_stream`, `connect_target`, `proxy_stream`, or `closed`) and, after a
successful handshake, include the authenticated peer, process-local
`connection_id`, connection age, selected relay/direct path, RTT, QUIC close
reason, and packet loss counters when they are available. Connection logs can
expose endpoint IDs, service names, targets, addresses, and network topology,
but never private key material.

With the currently pinned iroh 1.0.3 transport defaults, keep-alives run every
5 seconds, the connection idle timeout is 30 seconds, and direct/relay paths
use 15/30-second idle limits. The separate
`--close-on-request-timeout-secs` timer starts only after the local TCP request
side reaches EOF; it does not expire an active SSH stream.

A daemon implicitly started by an `add-*` command detaches with its standard
streams redirected to `/dev/null`. Install the user service or run `server` in
the foreground when persistent diagnostics are required.

### `status`

Check whether the live proxy server is running and show current counts.

```bash
iroh-proxy status
iroh-proxy status --connections
```

### `tui`

Open a ratatui dashboard for live server inspection and control.

```bash
iroh-proxy tui
```

Optional icon behavior:
- `USE_NERD_FONTS=1` enables Nerd Font icons in route tabs and pane titles
- `USE_NERD_FONTS=0` forces plain labels
- unset uses auto-detection (`TERM`/locale); plain labels are used when detection is not suitable

In the TUI:
- auto layout mode:
  - full mode at `>=160x36`: always-visible panes for `Services`, `Forwards`, and `Active Connections`
  - compact mode at `<160` columns or `<36` rows: routes table (`Services`/`Forwards`) stacked above `Connections`
- event-driven live updates from control-plane state signals
- dialogs to add/remove services and forwards
- remove-forward mode toggle:
  - runtime only (suspend listener)
  - runtime + config (persistent remove)

Keybinds:
- full mode: `Tab` / `Shift-Tab` change focused pane
- compact mode: `Tab` / `Shift-Tab` switch focused row (`Routes`/`Connections`)
- compact mode: `Left` / `Right` switch routes view (`Services`/`Forwards`)
- `Up` / `Down`: move selection
- when backend is up:
  - `a`: add in focused pane
  - `d`: remove selected item
- when backend is down:
  - `s`: start backend
- `r`: resync snapshot
- `q`: quit

### `add-serve`

Add a served TCP service route to the live proxy server.

If the server is not running, `add-serve` starts it in the background.

Service names must be 1-128 bytes and cannot contain `/`, whitespace, or control characters.

```bash
iroh-proxy add-serve <service-name> <target-host:port>
```

Use `-p` to persist the rule into config (`[serve].services`):

```bash
iroh-proxy add-serve -p <service-name> <target-host:port>
```

Example:

```bash
iroh-proxy add-serve -p ollama localhost:11434
```

### `del-serve`

Remove a served TCP service route from the running proxy server.

```bash
iroh-proxy del-serve <service-name>
```

Example:

```bash
iroh-proxy del-serve ollama
```

### `add-forward`

Add a local forward rule to the running proxy server.

If the server is not running, `add-forward` starts it in the background.

```bash
iroh-proxy add-forward <listen-host:port> <endpoint-id>/tcp/<service-name>
```

`add-forward` enables close-on-request mode by default, with a 2s timeout after local request upload EOF.
Tune it with:

```bash
iroh-proxy add-forward --close-on-request-timeout-secs <seconds> <listen-host:port> <endpoint-id>/tcp/<service-name>
```

Use `-p` to persist the rule into config (`[forward].services`):

```bash
iroh-proxy add-forward -p <listen-host:port> <endpoint-id>/tcp/<service-name>
```

Example:

```bash
iroh-proxy add-forward 127.0.0.1:5050 74f3645e8016bb34970c516acde5240e85ed4387dbe3aeb9189f50db5525bd76/tcp/app
```

### `del-forward`

Remove a forward rule from the running proxy server by its local listen address.
This stops the listener and also tears down any of its still-open forwarded
connections (it no longer leaves in-flight tunnels running in the background).

```bash
iroh-proxy del-forward <listen-host:port>
```

Use `-p` to also remove the matching rule from config (`[forward].services`):

```bash
iroh-proxy del-forward -p <listen-host:port>
```

Example:

```bash
iroh-proxy del-forward -p 127.0.0.1:11435
```

### `forward`

Forward to a remote iroh service path in two modes.

```bash
iroh-proxy forward <endpoint-id>/tcp/<service-name>
iroh-proxy forward <listen-host:port> <endpoint-id>/tcp/<service-name>
```

In listen mode, close-on-request is enabled by default (2s). Override with:

```bash
iroh-proxy forward --close-on-request-timeout-secs <seconds> <listen-host:port> <endpoint-id>/tcp/<service-name>
```

Connecting to the remote endpoint retries transient discovery failures,
timeouts, and peer resets with exponential backoff (up to 4 attempts, bounded
by a 30s total deadline). Permanent errors such as ALPN/TLS rejection fail
immediately. This applies to every forward mode (stdio, listener, `--fdpass`,
and daemon-managed `add-forward` rules) and covers connection establishment
only — an established stream that breaks is not resumed.

Examples:

```bash
# stdio mode (for ssh ProxyCommand)
iroh-proxy forward <endpoint-id>/tcp/ssh

# local listener mode
iroh-proxy forward 127.0.0.1:11435 <endpoint-id>/tcp/ollama
```

SSH example:

```sshconfig
Host gpu-iroh
    HostName ignored
    User your-user
    ProxyCommand iroh-proxy forward 74f3645e8016bb34970c516acde5240e85ed4387dbe3aeb9189f50db5525bd76/tcp/ssh
```

#### `--fdpass` (OpenSSH `ProxyUseFdpass yes`, Unix-only)

With `--fdpass`, `iroh-proxy forward` follows OpenSSH's file-descriptor-passing
convention instead of streaming bytes through stdio:

1. it creates an `AF_UNIX` `SOCK_STREAM` socketpair,
2. spawns a detached relay process holding one end (that end is what actually
   talks to iroh),
3. sends the other end back to `ssh` via `sendmsg(SCM_RIGHTS)` on stdout, then
   exits so `ssh`'s `waitpid()` returns and it can use the passed socket.

This avoids the extra hop through the parent process's pipes and hands ssh a
real kernel socket.

```sshconfig
Host gpu-iroh
    HostName ignored
    User your-user
    ProxyUseFdpass yes
    ProxyCommand iroh-proxy forward --fdpass 74f3645e8016bb34970c516acde5240e85ed4387dbe3aeb9189f50db5525bd76/tcp/ssh
```

Only valid in single-argument (remote-only) form. Not available on Windows.

### `forward-config`

Bind multiple local listeners from `config.toml`.

```bash
iroh-proxy forward-config ./config.toml
```

### `version`

Print version and build metadata.

```bash
iroh-proxy version
iroh-proxy version --short
```

## Config file

Default config path:

```text
~/.config/iroh-proxy/config.toml
```

See `config.example.toml`.

```toml
[serve]
[[serve.services]]
name = "ollama"
target = "localhost:11434"

[[serve.services]]
name = "vllm"
target = "localhost:8000"

[forward]
[[forward.services]]
listen = "127.0.0.1:11435"
remote = "<endpoint-id>/tcp/ollama"
close_on_request_timeout_secs = 2

[[forward.services]]
listen = "127.0.0.1:18000"
remote = "<endpoint-id>/tcp/vllm"
close_on_request_timeout_secs = 2
```

Sections are optional:
- `[serve]` is used by `server` and `add-serve -p`
- `[forward]` is used by `server` (persisted runtime listeners), `add-forward -p`, and `forward-config`

## End-to-end example

On service host:

```bash
iroh-proxy server
iroh-proxy add-serve -p ollama localhost:11434
```

Then get endpoint id:

```bash
iroh-proxy status
```

On client host:

```bash
iroh-proxy forward 127.0.0.1:11435 <endpoint-id>/tcp/ollama
```

Now local clients can use `127.0.0.1:11435` as if `ollama` were local on the service host.

## Keys and identity

- `server` uses a persistent key by default:
  - `~/.config/iroh-proxy/secret_key`
- `forward` and `forward-config` use an ephemeral in-memory key by default (new id each run).
- Use `--key-file` to force persistent key behavior for any command.

## Discovery

`iroh-proxy` uses discovery through:

- local mDNS (LAN discovery)
- pkarr over the n0 DNS service
- the Mainline DHT (relay addresses only by default)

## Notes and limits

- Supports only TCP forwarding paths in the form `<endpoint-id>/tcp/<name>`.
- Service names are limited to 1-128 bytes and cannot contain `/`, whitespace, or control characters.
- No authentication/authorization layer beyond iroh endpoint identity yet.
- No encryption-termination or HTTP-aware features; this is raw TCP stream proxying.
- Control API backend status:
  - Linux: DBus (`zbus`) implemented
  - macOS: sessionless `zbus` P2P over UDS (`$TMPDIR/iroh-proxy/control.sock`)
  - Windows: sessionless `zbus` P2P over UDS (`%TEMP%\\iroh-proxy\\control.sock`)
  - `status`/`tui`/`add-serve`/`add-forward`/`del-*` use the same control interface across these transports
  - On macOS the control socket is hardened: its directory is created `0700`, the socket file is `0600`, and connections from a different uid than the daemon's are rejected. On Windows the per-user `%TEMP%` directory is the boundary (no peer credentials are available over `uds_windows`).

## Development

```bash
cargo fmt
cargo check
cargo run -- --help
```

## License

TBD.
