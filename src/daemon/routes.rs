//! Serve-side routing: ALPN -> local TCP target, plus the incoming-connection
//! handler that pumps accepted iroh streams into the target.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, endpoint::Connection};
use tokio::net::TcpStream;
use tracing::warn;

use crate::proxy::{PumpStats, pump_streams};
use crate::remote_path::{service_to_alpn, validate_service_name};

use super::Routes;
use super::connections::{ActiveConnection, ConnectionRegistry};
use super::diagnostics::{
    IncomingFailure, IncomingStage, log_connection_accepted, log_connection_finished,
    log_established_failure, log_handshake_failure,
};

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

async fn proxy_to_route(
    conn: &Connection,
    route: &Route,
) -> std::result::Result<PumpStats, IncomingFailure> {
    let (send, recv) = conn.accept_bi().await.map_err(|err| {
        IncomingFailure::new(
            IncomingStage::AcceptStream,
            anyhow::Error::new(err).context("failed to accept first bidirectional iroh stream"),
        )
    })?;
    let local = TcpStream::connect(&*route.target).await.map_err(|err| {
        IncomingFailure::new(
            IncomingStage::ConnectTarget,
            anyhow::Error::new(err)
                .context(format!("failed to connect local target {}", route.target)),
        )
    })?;
    let (local_read, local_write) = local.into_split();

    // serve: up = target->iroh (response), down = iroh->target (request).
    // Per-direction half-close (no close-on-request timeout on the serve side).
    pump_streams(local_read, local_write, send, recv, None)
        .await
        .map_err(|err| {
            IncomingFailure::new(
                IncomingStage::ProxyStream,
                err.context("failed to proxy bidirectional stream"),
            )
        })
}

pub(super) async fn handle_incoming(
    incoming: iroh::endpoint::Incoming,
    routes: Routes,
    connections: ConnectionRegistry,
) {
    let transport_peer_addr = format!("{:?}", incoming.remote_addr());
    let transport_local_addr = format!("{:?}", incoming.local_addr());
    let conn = match incoming.await {
        Ok(conn) => conn,
        Err(err) => {
            let available_services = {
                let map = routes.read().await;
                map.values()
                    .map(|route| route.name.as_ref())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            log_handshake_failure(
                &transport_peer_addr,
                &transport_local_addr,
                &available_services,
                err,
            );
            return;
        }
    };
    let established_at = Instant::now();
    let negotiated = conn.alpn().to_vec();
    let (route, available_services) = {
        let map = routes.read().await;
        let route = map.get(&negotiated).cloned();
        let available = map
            .values()
            .map(|route| route.name.as_ref())
            .collect::<Vec<_>>()
            .join(",");
        (route, available)
    };
    let Some(route) = route else {
        warn!(
            stage = "route",
            connection_id = conn.stable_id(),
            peer = %conn.remote_id(),
            alpn = %String::from_utf8_lossy(&negotiated),
            available_services = %available_services,
            "incoming connection negotiated an unknown service"
        );
        return;
    };
    let initial_paths = log_connection_accepted(&conn, &route);

    let _guard = connections.register(ActiveConnection {
        src: conn.remote_id().to_string().into(),
        kind: "serve".into(),
        dst: route.name.clone(),
    });
    let pump_stats = match proxy_to_route(&conn, &route).await {
        Ok(stats) => stats,
        Err(failure) => {
            log_established_failure(
                &conn,
                &route,
                &initial_paths,
                established_at.elapsed(),
                failure,
            );
            return;
        }
    };

    // Let the requester drive the connection close so the final response bytes
    // are delivered before teardown (an eager drop here would truncate them).
    // Bounded by the connection idle timeout if the peer never closes.
    let close_reason = conn.closed().await;
    log_connection_finished(
        &conn,
        &route,
        &initial_paths,
        established_at.elapsed(),
        pump_stats,
        close_reason,
    );
}
