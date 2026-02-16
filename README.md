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
iroh-proxy [--key-file <path>] <command>
```

### `serve`

Expose one local TCP target under a named iroh service.

```bash
iroh-proxy serve --name <service-name> <target-host:port>
```

Example:

```bash
iroh-proxy serve --name ollama localhost:11434
```

### `serve-config`

Expose multiple local TCP targets from `config.toml`.

```bash
iroh-proxy serve-config ./config.toml
```

### `forward`

Forward to a remote iroh service path in two modes.

```bash
iroh-proxy forward <endpoint-id>/tcp/<service-name>
iroh-proxy forward <listen-host:port> <endpoint-id>/tcp/<service-name>
```

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

### `forward-config`

Bind multiple local listeners from `config.toml`.

```bash
iroh-proxy forward-config ./config.toml
```

## Config file

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

[[forward.services]]
listen = "127.0.0.1:18000"
remote = "<endpoint-id>/tcp/vllm"
```

You can include only the section needed by the command you run:

- `serve-config` requires `[serve]`
- `forward-config` requires `[forward]`

## End-to-end example (multi-service)

On GPU machine:

```bash
iroh-proxy serve-config ./config.toml
```

The process prints the endpoint id and all exported service paths:

```text
<endpoint-id>/tcp/ollama
<endpoint-id>/tcp/vllm
```

On client machine, put that endpoint id into `config.toml`, then run:

```bash
iroh-proxy forward-config ./config.toml
```

Now local clients can use each configured listener as if remote services were local.

## Keys and identity

- `serve` and `serve-config` use a persistent key by default:
  - `~/.config/iroh-proxy/secret_key`
- `forward` and `forward-config` use an ephemeral in-memory key by default (new id each run).
- Use `--key-file` to force persistent key behavior for any command.

## Discovery

`iroh-proxy` uses discovery through:

- local mDNS (LAN discovery)
- pkarr (relay + DHT)

## Notes and limits

- Supports only TCP forwarding paths in the form `<endpoint-id>/tcp/<name>`.
- No authentication/authorization layer beyond iroh endpoint identity yet.
- No encryption-termination or HTTP-aware features; this is raw TCP stream proxying.

## Development

```bash
cargo fmt
cargo check
cargo run -- --help
```

## License

TBD.
