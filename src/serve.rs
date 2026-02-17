use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use iroh::{
    Endpoint, RelayMode, SecretKey,
    address_lookup::{DhtAddressLookup, MdnsAddressLookup},
};
use std::io::ErrorKind;
use tokio::io::{self, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::config::ServeService;
use crate::remote_path::service_to_alpn;

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
struct Route {
    name: Box<str>,
    target: Box<str>,
}

pub async fn serve_services(secret_key: SecretKey, services: Vec<ServeService>) -> Result<()> {
    if services.is_empty() {
        bail!("at least one service is required");
    }

    let mut alpns = Vec::with_capacity(services.len());
    let mut routes = HashMap::<Vec<u8>, Route>::with_capacity(services.len());

    for service in services {
        if service.name.trim().is_empty() {
            bail!("service name cannot be empty");
        }

        let alpn = service_to_alpn(&service.name);
        if routes.contains_key(&alpn) {
            bail!("duplicate service name '{}'", service.name);
        }

        alpns.push(alpn.clone());
        routes.insert(
            alpn,
            Route {
                name: service.name,
                target: service.target,
            },
        );
    }

    let endpoint = Endpoint::empty_builder(RelayMode::Default)
        .secret_key(secret_key)
        .alpns(alpns)
        .address_lookup(DhtAddressLookup::builder().n0_dns_pkarr_relay())
        .address_lookup(MdnsAddressLookup::builder())
        .bind()
        .await?;

    endpoint.online().await;

    eprintln!("Serving endpoint id: {}", endpoint.id());
    eprintln!("Endpoint address: {:?}", endpoint.addr());
    for route in routes.values() {
        eprintln!("- {}/tcp/{} -> {}", endpoint.id(), route.name, route.target);
    }

    let routes = Arc::new(routes);
    loop {
        let incoming = endpoint.accept().await.context("endpoint closed")?;
        let routes = Arc::clone(&routes);

        tokio::spawn(async move {
            if let Err(err) = handle_incoming(incoming, routes).await {
                eprintln!("incoming connection error: {err:#}");
            }
        });
    }
}

async fn handle_incoming(
    incoming: iroh::endpoint::Incoming,
    routes: Arc<HashMap<Vec<u8>, Route>>,
) -> Result<()> {
    let conn = incoming.await?;
    let negotiated = conn.alpn();

    let route = routes
        .get(negotiated)
        .ok_or_else(|| {
            anyhow!(
                "unknown service ALPN '{}'",
                String::from_utf8_lossy(negotiated)
            )
        })?
        .clone();

    let (mut send, mut recv) = conn.accept_bi().await?;
    let local = TcpStream::connect(&*route.target)
        .await
        .with_context(|| format!("failed to connect local target {}", route.target))?;

    let (mut local_read, mut local_write) = local.into_split();

    let remote_to_local = async {
        match io::copy(&mut recv, &mut local_write).await {
            Ok(bytes) => {
                info!(bytes, service = %route.name, "serve iroh->tcp reached EOF (client half-closed or disconnected)")
            }
            Err(err) => {
                if is_disconnect(&err) {
                    warn!(error = %err, service = %route.name, "serve iroh->tcp disconnected");
                } else {
                    warn!(error = %err, service = %route.name, "serve iroh->tcp copy failed");
                }
                return Err(err.into());
            }
        }
        local_write.shutdown().await?;
        info!(service = %route.name, "serve half-closed local tcp write after iroh EOF");
        Ok::<(), anyhow::Error>(())
    };
    let local_to_remote = async {
        match io::copy(&mut local_read, &mut send).await {
            Ok(bytes) => {
                info!(bytes, service = %route.name, "serve tcp->iroh reached EOF (target half-closed or disconnected)")
            }
            Err(err) => {
                if is_disconnect(&err) {
                    warn!(error = %err, service = %route.name, "serve tcp->iroh disconnected");
                } else {
                    warn!(error = %err, service = %route.name, "serve tcp->iroh copy failed");
                }
                return Err(err.into());
            }
        }
        if let Err(err) = send.finish() {
            warn!(error = %err, service = %route.name, "serve failed to half-close iroh send stream");
        } else {
            info!(service = %route.name, "serve half-closed iroh send stream after tcp EOF");
        }
        Ok::<(), anyhow::Error>(())
    };

    let _ = tokio::try_join!(remote_to_local, local_to_remote)?;

    Ok(())
}
