//! Shared forwarding core.
//!
//! This module single-sources the three things that used to be duplicated
//! between the daemon and the standalone `forward` paths:
//!
//! - [`build_endpoint`]: iroh `Endpoint` construction (server publishes, client
//!   does not).
//! - [`connect_remote`]: connecting to a remote service path.
//! - [`pump_streams`]: the bidirectional copy with per-direction half-close and
//!   an optional close-on-request timeout.
//!
//! [`pump_streams`] is generic over [`AsyncRead`]/[`AsyncWrite`] so it can be
//! unit-tested with [`tokio::io::duplex`] without standing up a network. The
//! iroh `SendStream` implements `AsyncWrite` (its `poll_shutdown` calls
//! `finish()`), so half-closing the QUIC send stream is just `shutdown().await`.

use std::time::Duration;

use anyhow::{Context, Result};
use iroh::{
    Endpoint, RelayMode, SecretKey,
    address_lookup::{DhtAddressLookup, MdnsAddressLookup},
};
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::remote_path::RemotePath;

/// Classifies a copy error as a benign, peer-initiated disconnect (logged at
/// `debug`) rather than a genuine failure (logged at `warn`).
pub(crate) fn is_disconnect(err: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        err.kind(),
        BrokenPipe | ConnectionReset | ConnectionAborted | UnexpectedEof | NotConnected
    )
}

/// Build an iroh endpoint.
///
/// When `publish` is `true` (server side) the endpoint advertises itself via
/// DHT/pkarr and mDNS so it is discoverable. When `false` (client/forward side)
/// it neither publishes to pkarr nor advertises over mDNS.
pub async fn build_endpoint(secret_key: SecretKey, publish: bool) -> Result<Endpoint> {
    let dht = DhtAddressLookup::builder().n0_dns_pkarr_relay();
    let dht = if publish { dht } else { dht.no_publish() };
    let mdns = MdnsAddressLookup::builder();
    let mdns = if publish { mdns } else { mdns.advertise(false) };

    let endpoint = Endpoint::empty_builder(RelayMode::Default)
        .secret_key(secret_key)
        .address_lookup(dht)
        .address_lookup(mdns)
        .bind()
        .await?;
    endpoint.online().await;
    Ok(endpoint)
}

/// Connect to a remote service path over iroh.
pub async fn connect_remote(
    endpoint: &Endpoint,
    remote: &RemotePath,
) -> Result<iroh::endpoint::Connection> {
    let alpn = remote.to_alpn();
    endpoint
        .connect(remote.endpoint_id, &alpn)
        .await
        .with_context(|| {
            format!(
                "failed connecting to remote endpoint {} (discovery failed via local mDNS and pkarr)",
                remote.endpoint_id
            )
        })
}

/// Forward one accepted local TCP connection to a remote service: connect,
/// open a bi-stream, pump until EOF/timeout, and close the iroh connection on
/// every exit path.
///
/// `register` is called with the peer address once the remote connection is
/// established; whatever it returns is held until the transfer finishes. The
/// daemon passes a closure that registers the connection in its live registry
/// (the guard's `Drop` unregisters it); the standalone `forward` passes one
/// returning `()`.
pub async fn forward_tcp_conn<G>(
    endpoint: &Endpoint,
    inbound: TcpStream,
    remote: &RemotePath,
    close_on_request_timeout: Duration,
    register: impl FnOnce(&str) -> G,
) -> Result<()> {
    let src = inbound
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    info!(
        target: "iroh_proxy::forward",
        src = %src,
        remote = %remote.endpoint_id,
        service = %remote.service,
        "forwarding connection"
    );
    let conn = connect_remote(endpoint, remote).await?;
    let _guard = register(&src);
    let (send, recv) = conn.open_bi().await?;
    info!(
        target: "iroh_proxy::forward",
        src = %src,
        remote = %remote.endpoint_id,
        service = %remote.service,
        "forward stream established"
    );

    let (inbound_read, inbound_write) = inbound.into_split();
    let result = pump_streams(
        inbound_read,
        inbound_write,
        send,
        recv,
        Some(close_on_request_timeout),
    )
    .await;

    // Guarantee teardown of the iroh connection on every exit path (in
    // particular the timeout/error paths).
    conn.close(0u32.into(), b"closed");

    let stats = result?;
    if stats.timed_out {
        warn!(
            target: "iroh_proxy::forward",
            src = %src,
            remote = %remote.endpoint_id,
            service = %remote.service,
            timeout_secs = close_on_request_timeout.as_secs_f64(),
            "forward close-on-request timeout reached; connection closed"
        );
    } else {
        info!(
            target: "iroh_proxy::forward",
            src = %src,
            remote = %remote.endpoint_id,
            service = %remote.service,
            up_bytes = stats.up_bytes,
            down_bytes = stats.down_bytes,
            "forwarding finished"
        );
    }
    Ok(())
}

/// Result of a [`pump_streams`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PumpStats {
    /// Bytes copied local -> remote (the "upload"/request direction).
    pub up_bytes: u64,
    /// Bytes copied remote -> local (the "download"/response direction).
    pub down_bytes: u64,
    /// True if the close-on-request timeout fired and the response direction
    /// was aborted before it completed.
    pub timed_out: bool,
}

/// Pump bytes in both directions between a local stream (TCP / stdio / unix) and
/// the remote iroh bi-stream halves.
///
/// Directions:
/// - **up**: `local_read -> remote_send`. On EOF the remote send stream is
///   half-closed (`shutdown`, which finishes the QUIC stream).
/// - **down**: `remote_recv -> local_write`. On EOF the local write half is
///   shut down.
///
/// The down direction runs in its own task so it can be bounded and, crucially,
/// **aborted**:
/// - `close_on_request_timeout = Some(d)` (client forward semantics): after the
///   up direction reaches EOF, the down direction is given `d` to finish; if it
///   does not, its task is aborted and `timed_out` is set. Dropping a
///   `JoinHandle` only *detaches* the task — we must `abort()` to actually stop
///   the copy and release the iroh `RecvStream`.
/// - `close_on_request_timeout = None` (serve / stdio / fdpass semantics): both
///   directions run to completion.
///
/// If the up direction errors, the down task is aborted before returning so a
/// failed upload does not leak a detached copy task (and the open connection).
pub async fn pump_streams<LR, LW, RW, RR>(
    mut local_read: LR,
    local_write: LW,
    mut remote_send: RW,
    remote_recv: RR,
    close_on_request_timeout: Option<Duration>,
) -> Result<PumpStats>
where
    LR: AsyncRead + Unpin + Send,
    LW: AsyncWrite + Unpin + Send + 'static,
    RW: AsyncWrite + Unpin + Send,
    RR: AsyncRead + Unpin + Send + 'static,
{
    // down: remote_recv -> local_write, spawned so it is abortable.
    let mut down: JoinHandle<Result<u64>> = tokio::spawn(async move {
        let mut remote_recv = remote_recv;
        let mut local_write = local_write;
        let n = match io::copy(&mut remote_recv, &mut local_write).await {
            Ok(n) => n,
            Err(err) => {
                if is_disconnect(&err) {
                    debug!(error = %err, "iroh->local disconnected");
                } else {
                    warn!(error = %err, "iroh->local copy failed");
                }
                return Err(err.into());
            }
        };
        let _ = local_write.shutdown().await;
        Ok(n)
    });

    // up: local_read -> remote_send (inline), then half-close the send stream.
    let up_result: Result<u64> = async {
        let n = match io::copy(&mut local_read, &mut remote_send).await {
            Ok(n) => n,
            Err(err) => {
                if is_disconnect(&err) {
                    debug!(error = %err, "local->iroh disconnected");
                } else {
                    warn!(error = %err, "local->iroh copy failed");
                }
                return Err(err.into());
            }
        };
        if let Err(err) = remote_send.shutdown().await {
            debug!(error = %err, "failed to half-close iroh send stream");
        }
        Ok(n)
    }
    .await;

    let up_bytes = match up_result {
        Ok(n) => n,
        Err(err) => {
            // Tear down the sibling so a failed upload does not leak a task.
            down.abort();
            return Err(err);
        }
    };

    let mut timed_out = false;
    let down_bytes = match close_on_request_timeout {
        Some(timeout) => match tokio::time::timeout(timeout, &mut down).await {
            Ok(joined) => joined.context("iroh->local task panicked")??,
            Err(_) => {
                down.abort();
                timed_out = true;
                0
            }
        },
        None => (&mut down).await.context("iroh->local task panicked")??,
    };

    Ok(PumpStats {
        up_bytes,
        down_bytes,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex, split};
    use tokio::net::{TcpListener, TcpStream};

    /// Unidirectional pipe built from a duplex pair: `tx` is written by the test,
    /// `rx` is read by the code under test (or vice versa).
    fn pipe() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
        duplex(64 * 1024)
    }

    #[tokio::test]
    async fn roundtrips_both_directions_with_half_close() {
        // local_read side: test writes the "request"
        let (mut app_to_proxy, local_read) = pipe();
        // remote_send side: test reads what the proxy forwarded upstream
        let (remote_send, mut upstream_in) = pipe();
        // remote_recv side: test writes the "response" coming from upstream
        let (mut upstream_out, remote_recv) = pipe();
        // local_write side: test reads what the proxy delivered back locally
        let (local_write, mut proxy_to_app) = pipe();

        let pump = tokio::spawn(pump_streams(
            local_read,
            local_write,
            remote_send,
            remote_recv,
            None,
        ));

        // Drive the request: send then EOF.
        app_to_proxy.write_all(b"ping").await.unwrap();
        app_to_proxy.shutdown().await.unwrap();
        drop(app_to_proxy);

        // Proxy should forward the request upstream, then half-close.
        let mut got = Vec::new();
        upstream_in.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"ping");

        // Drive the response: send then EOF.
        upstream_out.write_all(b"pong").await.unwrap();
        upstream_out.shutdown().await.unwrap();
        drop(upstream_out);

        let mut back = Vec::new();
        proxy_to_app.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"pong");

        let stats = pump.await.unwrap().unwrap();
        assert_eq!(stats.up_bytes, 4);
        assert_eq!(stats.down_bytes, 4);
        assert!(!stats.timed_out);
    }

    #[tokio::test]
    async fn close_on_request_timeout_actually_stops_the_response() {
        // Regression test for the bug where the timeout branch merely dropped
        // (detached) the response task instead of aborting it: the pump would
        // never return until the response side independently closed.
        let (mut app_to_proxy, local_read) = pipe();
        let (remote_send, mut upstream_in) = pipe();
        // Keep both ends of the response channel alive so the down copy would
        // block forever if it were not aborted on timeout.
        let (_upstream_out_held, remote_recv) = pipe();
        let (local_write, _proxy_to_app_held) = pipe();

        let pump = tokio::spawn(pump_streams(
            local_read,
            local_write,
            remote_send,
            remote_recv,
            Some(Duration::from_millis(100)),
        ));

        // Finish the request so the timeout window opens.
        app_to_proxy.write_all(b"req").await.unwrap();
        app_to_proxy.shutdown().await.unwrap();
        drop(app_to_proxy);
        let mut got = Vec::new();
        upstream_in.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"req");

        // Without the abort fix this would hang; bound it generously.
        let stats = tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("pump must return promptly after the close-on-request timeout")
            .unwrap()
            .unwrap();
        assert!(stats.timed_out, "expected the timeout branch to fire");
        assert_eq!(stats.up_bytes, 3);
    }

    #[test]
    fn is_disconnect_classifies_benign_kinds() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::UnexpectedEof,
            ErrorKind::NotConnected,
        ] {
            assert!(
                is_disconnect(&Error::from(kind)),
                "{kind:?} should be benign"
            );
        }
        assert!(!is_disconnect(&Error::from(ErrorKind::PermissionDenied)));
        assert!(!is_disconnect(&Error::from(ErrorKind::InvalidData)));
    }

    /// End-to-end: bytes round-trip through two real in-process iroh endpoints,
    /// exercising both the serve-side pump (`None`) and the forward-side pump
    /// (`Some`) over a real QUIC connection plus a real TCP echo target.
    /// Uses `RelayMode::Disabled` and connects by direct address, so it needs no
    /// network/discovery.
    #[tokio::test]
    async fn end_to_end_roundtrip_over_iroh() -> Result<()> {
        // TCP echo server acting as the proxied target.
        let echo = TcpListener::bind("127.0.0.1:0").await?;
        let echo_addr = echo.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = sock.split();
                    let _ = io::copy(&mut r, &mut w).await;
                });
            }
        });

        let alpn = b"iroh-proxy/tcp/echo".to_vec();

        // Server endpoint: serve path via pump_streams(None) into the echo.
        let server = Endpoint::empty_builder(RelayMode::Disabled)
            .secret_key(SecretKey::generate(&mut StdRng::from_os_rng()))
            .bind()
            .await?;
        server.set_alpns(vec![alpn.clone()]);

        // Dial by direct loopback address (no relay / no discovery): build an
        // EndpointAddr from the server's bound UDP sockets, mapping the
        // unspecified bind address to localhost.
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
        let mut server_addr = iroh::EndpointAddr::new(server.id());
        for sock in server.bound_sockets() {
            let ip = match sock.ip() {
                IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                other => other,
            };
            server_addr = server_addr.with_ip_addr(SocketAddr::new(ip, sock.port()));
        }

        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.context("no incoming connection")?;
            let conn = incoming.await?;
            let (send, recv) = conn.accept_bi().await?;
            let target = TcpStream::connect(echo_addr).await?;
            let (tr, tw) = target.into_split();
            let stats = pump_streams(tr, tw, send, recv, None).await?;
            // Wait for the requester to close so the response is fully delivered.
            conn.closed().await;
            Ok::<PumpStats, anyhow::Error>(stats)
        });

        // Client endpoint: forward path via pump_streams(Some) over a duplex.
        // The whole exchange is bounded so a discovery/connectivity failure
        // fails the test instead of hanging the suite.
        let exchange = async {
            let client = Endpoint::empty_builder(RelayMode::Disabled)
                .secret_key(SecretKey::generate(&mut StdRng::from_os_rng()))
                .bind()
                .await?;
            let conn = client.connect(server_addr, &alpn).await?;
            let (send, recv) = conn.open_bi().await?;

            let (mut app, local) = duplex(64 * 1024);
            let (local_read, local_write) = split(local);
            let client_pump = tokio::spawn(pump_streams(
                local_read,
                local_write,
                send,
                recv,
                Some(Duration::from_secs(10)),
            ));

            let payload = b"hello over iroh".to_vec();
            app.write_all(&payload).await?;
            app.shutdown().await?;
            let mut got = Vec::new();
            app.read_to_end(&mut got).await?;

            conn.close(0u32.into(), b"done");
            let client_stats = client_pump.await?.expect("client pump ok");
            assert_eq!(
                got, payload,
                "payload must round-trip through the echo target"
            );
            assert!(!client_stats.timed_out);
            assert_eq!(client_stats.up_bytes, payload.len() as u64);
            assert_eq!(client_stats.down_bytes, payload.len() as u64);
            Ok::<(), anyhow::Error>(())
        };

        tokio::time::timeout(Duration::from_secs(30), exchange)
            .await
            .context("e2e exchange timed out (iroh connectivity unavailable?)")??;
        server_task.await?.expect("server pump ok");
        Ok(())
    }
}
