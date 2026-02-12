use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use iroh::{Endpoint, SecretKey};
use tokio::io;
use tokio::net::TcpStream;

use crate::config::ServeService;
use crate::remote_path::service_to_alpn;

#[derive(Debug, Clone)]
struct Route {
    name: String,
    target: String,
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

    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .alpns(alpns)
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
    let local = TcpStream::connect(&route.target)
        .await
        .with_context(|| format!("failed to connect local target {}", route.target))?;

    let (mut local_read, mut local_write) = local.into_split();

    let to_local = io::copy(&mut recv, &mut local_write);
    let to_remote = io::copy(&mut local_read, &mut send);
    let _ = tokio::try_join!(to_local, to_remote)?;

    send.finish()?;
    Ok(())
}
