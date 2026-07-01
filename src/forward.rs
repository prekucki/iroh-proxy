use anyhow::{Context, Result, bail};
use iroh::SecretKey;
use std::time::Duration;
use tokio::io;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::proxy::{
    RetryPolicy, build_endpoint, connect_remote_with_retry, forward_tcp_conn, pump_streams,
};
use crate::remote_path::RemotePath;

#[derive(Debug, Clone)]
pub struct ForwardBinding {
    pub listen: Box<str>,
    pub remote: RemotePath,
    pub close_on_request_timeout: Duration,
}

pub async fn forward_stdio(secret_key: SecretKey, remote: RemotePath) -> Result<()> {
    let endpoint = build_endpoint(secret_key, false).await?;
    let conn = connect_remote_with_retry(&endpoint, &remote, RetryPolicy::default()).await?;
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
            if let Err(err) = forward_tcp_conn(
                &endpoint,
                inbound,
                &remote,
                close_on_request_timeout,
                |_| (),
            )
            .await
            {
                warn!(peer = %peer_addr, error = %format!("{err:#}"), "forwarding connection failed");
            }
        });
    }
}
