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

Expose a local TCP target under a named iroh service.

```bash
iroh-proxy serve --name <service-name> <target-host:port>
```

Example:

```bash
iroh-proxy serve --name ollama localhost:11434
```

This prints an endpoint id and serves the path:

```text
<endpoint-id>/tcp/ollama
```

### `forward`

Bind a local TCP listener and forward to a remote iroh service path.

```bash
iroh-proxy forward <listen-host:port> <endpoint-id>/tcp/<service-name>
```

Example:

```bash
iroh-proxy forward 127.0.0.1:11435 <endpoint-id>/tcp/ollama
```

Now clients can use `127.0.0.1:11435` as if the service were local.

## End-to-end example (Ollama)

On GPU machine:

```bash
iroh-proxy serve --name ollama localhost:11434
```

On client machine:

```bash
iroh-proxy forward 127.0.0.1:11435 <endpoint-id>/tcp/ollama
```

Then call Ollama via:

```text
http://127.0.0.1:11435
```

## Keys and identity

- By default, a persistent iroh secret key is stored at:
  - `~/.config/iroh-proxy/secret_key`
- Use `--key-file` to override.
- Keeping the same key keeps the same endpoint id.

## Notes and limits

- Supports only TCP forwarding paths in the form `<endpoint-id>/tcp/<name>`.
- No authentication/authorization layer beyond iroh endpoint identity yet.
- No encryption-termination or HTTP-aware features; this is raw TCP stream proxying.

## Development

```bash
cargo check
cargo test
cargo run -- --help
```

## License

TBD.
