//! Serve-side routing: ALPN -> local TCP target, plus the incoming-connection
//! handler that pumps accepted iroh streams into the target.

use anyhow::{Context, Result, anyhow, bail};
use iroh::Endpoint;
use tokio::net::TcpStream;
use tracing::info;

use crate::proxy::pump_streams;
use crate::remote_path::{service_to_alpn, validate_service_name};

use super::Routes;
use super::connections::{ActiveConnection, ConnectionRegistry};

#[derive(Debug, Clone)]
pub(super) struct Route {
    pub(super) name: Box<str>,
    pub(super) target: Box<str>,
}

pub(super) async fn add_serve_route(routes: &Routes, name: &str, target: &str) -> Result<()> {
    validate_service_name(name).with_context(|| format!("invalid service name '{name}'"))?;
    if target.trim().is_empty() {
        bail!("target cannot be empty");
    }

    let alpn = service_to_alpn(name);
    let mut map = routes.write().await;
    if map.contains_key(&alpn) {
        bail!("duplicate service name '{}'", name);
    }
    map.insert(
        alpn,
        Route {
            name: name.into(),
            target: target.into(),
        },
    );
    Ok(())
}

pub(super) async fn sync_endpoint_alpns(endpoint: &Endpoint, routes: &Routes) {
    let alpns = {
        let map = routes.read().await;
        map.keys().cloned().collect::<Vec<_>>()
    };
    endpoint.set_alpns(alpns);
}

pub(super) async fn handle_incoming(
    incoming: iroh::endpoint::Incoming,
    routes: Routes,
    connections: ConnectionRegistry,
) -> Result<()> {
    let conn = incoming.await?;
    let negotiated = conn.alpn().to_vec();
    let route = {
        let map = routes.read().await;
        map.get(&negotiated).cloned().ok_or_else(|| {
            anyhow!(
                "unknown service ALPN '{}'",
                String::from_utf8_lossy(&negotiated)
            )
        })?
    };
    info!(
        peer = %conn.remote_id(),
        service = %route.name,
        "accepted incoming connection"
    );

    let _guard = connections.register(ActiveConnection {
        src: conn.remote_id().to_string().into(),
        kind: "serve".into(),
        dst: route.name.clone(),
    });
    let (send, recv) = conn.accept_bi().await?;
    let local = TcpStream::connect(&*route.target)
        .await
        .with_context(|| format!("failed to connect local target {}", route.target))?;
    let (local_read, local_write) = local.into_split();

    // serve: up = target->iroh (response), down = iroh->target (request).
    // Per-direction half-close (no close-on-request timeout on the serve side).
    pump_streams(local_read, local_write, send, recv, None).await?;

    // Let the requester drive the connection close so the final response bytes
    // are delivered before teardown (an eager drop here would truncate them).
    // Bounded by the connection idle timeout if the peer never closes.
    conn.closed().await;
    Ok(())
}
