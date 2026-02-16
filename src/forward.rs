use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint, RelayMode, SecretKey,
    address_lookup::{DhtAddressLookup, MdnsAddressLookup},
};
use tokio::io::{self, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::remote_path::RemotePath;

#[derive(Debug, Clone)]
pub struct ForwardBinding {
    pub listen: Box<str>,
    pub remote: RemotePath,
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

    let to_remote = io::copy(&mut stdin, &mut send);
    let to_local = io::copy(&mut recv, &mut stdout);
    let _ = tokio::try_join!(to_remote, to_local)?;

    send.finish()?;
    stdout.flush().await?;
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

        tokio::spawn(async move {
            if let Err(err) = handle_forward_conn(endpoint, inbound, remote, alpn).await {
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
) -> Result<()> {
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

    let (mut inbound_read, mut inbound_write) = inbound.into_split();

    let to_remote = io::copy(&mut inbound_read, &mut send);
    let to_local = io::copy(&mut recv, &mut inbound_write);
    let _ = tokio::try_join!(to_remote, to_local)?;

    send.finish()?;
    Ok(())
}
