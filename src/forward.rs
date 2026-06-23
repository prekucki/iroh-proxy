use anyhow::{Context, Result, bail};
use iroh::SecretKey;
use std::time::Duration;
use tokio::io;
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::proxy::{build_endpoint, connect_remote, pump_streams};
use crate::remote_path::RemotePath;

#[derive(Debug, Clone)]
pub struct ForwardBinding {
    pub listen: Box<str>,
    pub remote: RemotePath,
    pub close_on_request_timeout: Duration,
}

pub async fn forward_stdio(secret_key: SecretKey, remote: RemotePath) -> Result<()> {
    let endpoint = build_endpoint(secret_key, false).await?;
    let conn = connect_remote(&endpoint, &remote).await?;
    let (send, recv) = conn.open_bi().await?;

    // stdio mode: run both directions to completion (no close-on-request timeout).
    let result = pump_streams(io::stdin(), io::stdout(), send, recv, None).await;
    conn.close(0u32.into(), b"closed");
    let stats = result?;
    info!(
        up_bytes = stats.up_bytes,
        down_bytes = stats.down_bytes,
        "forward stdio finished"
    );
    Ok(())
}

pub async fn forward_bindings(secret_key: SecretKey, bindings: Vec<ForwardBinding>) -> Result<()> {
    if bindings.is_empty() {
        bail!("at least one forward binding is required");
    }

    let endpoint = build_endpoint(secret_key, false).await?;

    let mut prepared = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let listener = TcpListener::bind(&*binding.listen)
            .await
            .with_context(|| format!("failed to bind local listener {}", binding.listen))?;
        info!(
            listen = %binding.listen,
            remote = %binding.remote.endpoint_id,
            service = %binding.remote.service,
            "forwarding"
        );
        prepared.push((listener, binding));
    }

    for (listener, binding) in prepared {
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            if let Err(err) = run_forward_listener(endpoint, listener, binding).await {
                warn!(error = %format!("{err:#}"), "forward listener error");
            }
        });
    }

    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}

async fn run_forward_listener(
    endpoint: iroh::Endpoint,
    listener: TcpListener,
    binding: ForwardBinding,
) -> Result<()> {
    loop {
        let (inbound, peer_addr) = listener.accept().await?;
        let endpoint = endpoint.clone();
        let remote = binding.remote.clone();
        let close_on_request_timeout = binding.close_on_request_timeout;

        tokio::spawn(async move {
            if let Err(err) =
                handle_forward_conn(endpoint, inbound, remote, close_on_request_timeout).await
            {
                warn!(peer = %peer_addr, error = %format!("{err:#}"), "forwarding connection failed");
            }
        });
    }
}

async fn handle_forward_conn(
    endpoint: iroh::Endpoint,
    inbound: TcpStream,
    remote: RemotePath,
    close_on_request_timeout: Duration,
) -> Result<()> {
    let src = inbound
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());

    let conn = connect_remote(&endpoint, &remote).await?;
    let (send, recv) = conn.open_bi().await?;
    info!(
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
    // particular the timeout/error paths, which the old code failed to close).
    conn.close(0u32.into(), b"closed");

    let stats = result?;
    if stats.timed_out {
        warn!(
            src = %src,
            remote = %remote.endpoint_id,
            service = %remote.service,
            timeout_secs = close_on_request_timeout.as_secs_f64(),
            "forward close-on-request timeout reached; connection closed"
        );
    } else {
        info!(
            src = %src,
            up_bytes = stats.up_bytes,
            down_bytes = stats.down_bytes,
            "forwarding finished"
        );
    }
    Ok(())
}
