use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint, RelayMode, SecretKey,
    address_lookup::{DhtAddressLookup, MdnsAddressLookup},
};
use std::io::ErrorKind;
use std::time::Duration;
use tokio::io::{self, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::remote_path::RemotePath;

fn is_disconnect(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::BrokenPipe
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::UnexpectedEof
            | ErrorKind::NotConnected
    )
}

#[derive(Debug, Clone)]
pub struct ForwardBinding {
    pub listen: Box<str>,
    pub remote: RemotePath,
    pub close_on_request_timeout: Duration,
}

async fn build_forward_endpoint(secret_key: SecretKey) -> Result<Endpoint> {
    let endpoint = Endpoint::empty_builder(RelayMode::Default)
        .secret_key(secret_key)
        .address_lookup(
            DhtAddressLookup::builder()
                .n0_dns_pkarr_relay()
                .no_publish(),
        )
        .address_lookup(MdnsAddressLookup::builder().advertise(false))
        .bind()
        .await?;
    endpoint.online().await;
    Ok(endpoint)
}

pub async fn forward_stdio(secret_key: SecretKey, remote: RemotePath) -> Result<()> {
    let alpn = remote.to_alpn();
    let endpoint = build_forward_endpoint(secret_key).await?;

    let conn = endpoint
        .connect(remote.endpoint_id, &alpn)
        .await
        .with_context(|| {
            format!(
                "failed connecting to remote endpoint {} (discovery failed via local mDNS and pkarr)",
                remote.endpoint_id
            )
        })?;
    let (mut send, mut recv) = conn.open_bi().await?;

    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    let stdin_to_remote = async {
        match io::copy(&mut stdin, &mut send).await {
            Ok(bytes) => info!(bytes, "forward stdio stdin->iroh reached EOF"),
            Err(err) => {
                if is_disconnect(&err) {
                    warn!(error = %err, "forward stdio stdin->iroh disconnected");
                } else {
                    warn!(error = %err, "forward stdio stdin->iroh copy failed");
                }
                return Err(err.into());
            }
        }
        if let Err(err) = send.finish() {
            warn!(error = %err, "forward stdio failed to half-close iroh send stream");
        } else {
            info!("forward stdio half-closed iroh send stream");
        }
        Ok::<(), anyhow::Error>(())
    };
    let remote_to_stdout = async {
        match io::copy(&mut recv, &mut stdout).await {
            Ok(bytes) => info!(bytes, "forward stdio iroh->stdout reached EOF"),
            Err(err) => {
                if is_disconnect(&err) {
                    warn!(error = %err, "forward stdio iroh->stdout disconnected");
                } else {
                    warn!(error = %err, "forward stdio iroh->stdout copy failed");
                }
                return Err(err.into());
            }
        }
        stdout.flush().await?;
        Ok::<(), anyhow::Error>(())
    };

    let _ = tokio::try_join!(stdin_to_remote, remote_to_stdout)?;

    Ok(())
}

pub async fn forward_bindings(secret_key: SecretKey, bindings: Vec<ForwardBinding>) -> Result<()> {
    if bindings.is_empty() {
        bail!("at least one forward binding is required");
    }

    let endpoint = build_forward_endpoint(secret_key).await?;

    let mut prepared = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let listener = TcpListener::bind(&*binding.listen)
            .await
            .with_context(|| format!("failed to bind local listener {}", binding.listen))?;

        let alpn = binding.remote.to_alpn();
        eprintln!(
            "Forwarding {} -> {}/tcp/{}",
            binding.listen, binding.remote.endpoint_id, binding.remote.service
        );

        prepared.push((listener, binding, alpn));
    }

    for (listener, binding, alpn) in prepared {
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            if let Err(err) = run_forward_listener(endpoint, listener, binding, alpn).await {
                eprintln!("forward listener error: {err:#}");
            }
        });
    }

    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}

async fn run_forward_listener(
    endpoint: Endpoint,
    listener: TcpListener,
    binding: ForwardBinding,
    alpn: Vec<u8>,
) -> Result<()> {
    loop {
        let (inbound, peer_addr) = listener.accept().await?;
        let endpoint = endpoint.clone();
        let remote = binding.remote.clone();
        let alpn = alpn.clone();
        let close_on_request_timeout = binding.close_on_request_timeout;

        tokio::spawn(async move {
            if let Err(err) =
                handle_forward_conn(endpoint, inbound, remote, alpn, close_on_request_timeout).await
            {
                eprintln!("forwarding {peer_addr} failed: {err:#}");
            }
        });
    }
}

async fn handle_forward_conn(
    endpoint: Endpoint,
    inbound: TcpStream,
    remote: RemotePath,
    alpn: Vec<u8>,
    close_on_request_timeout: Duration,
) -> Result<()> {
    info!("forwarding");

    let conn = endpoint
        .connect(remote.endpoint_id, &alpn)
        .await
        .with_context(|| {
            format!(
                "failed connecting to remote endpoint {} (discovery failed via local mDNS and pkarr)",
                remote.endpoint_id
            )
        })?;

    let (mut send, mut recv) = conn.open_bi().await?;

    info!("forward tcp->iroh connected");

    let (mut inbound_read, mut inbound_write) = inbound.into_split();

    let inbound_to_remote = async move {
        warn!("forward tcp->iroh started");
        match io::copy(&mut inbound_read, &mut send).await {
            Ok(bytes) => info!(
                bytes,
                "forward tcp->iroh reached EOF (client half-closed or disconnected)"
            ),
            Err(err) => {
                if is_disconnect(&err) {
                    warn!(error = %err, "forward tcp->iroh disconnected");
                } else {
                    warn!(error = %err, "forward tcp->iroh copy failed");
                }
                return Err(err.into());
            }
        }
        if let Err(err) = send.finish() {
            warn!(error = %err, "forward failed to half-close iroh send stream");
        } else {
            info!("forward half-closed iroh send stream after tcp EOF");
        }
        drop(inbound_read);
        Ok::<(), anyhow::Error>(())
    };
    let remote_to_inbound: JoinHandle<Result<()>> = tokio::spawn(async move {
        match io::copy(&mut recv, &mut inbound_write).await {
            Ok(bytes) => info!(
                bytes,
                "forward iroh->tcp reached EOF (remote half-closed or disconnected)"
            ),
            Err(err) => {
                if is_disconnect(&err) {
                    warn!(error = %err, "forward iroh->tcp disconnected");
                } else {
                    warn!(error = %err, "forward iroh->tcp copy failed");
                }
                return Err(err.into());
            }
        }
        drop(recv);
        drop(inbound_write);
        info!("forward half-closed local tcp write after iroh EOF");
        Ok::<(), anyhow::Error>(())
    });

    inbound_to_remote.await?;
    match tokio::time::timeout(close_on_request_timeout, remote_to_inbound).await {
        Ok(joined) => joined??,
        Err(_) => {
            warn!(
                timeout_secs = close_on_request_timeout.as_secs_f64(),
                "forward close-on-request timeout reached; closing connection"
            );
        }
    }

    Ok(())
}
