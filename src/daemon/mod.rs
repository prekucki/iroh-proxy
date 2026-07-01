//! The `server` daemon: iroh endpoint, serve routes, forward bindings, the
//! connection registry, and the platform control plane.
//!
//! Module map:
//! - [`connections`]: registry of in-flight connections (drop-guard based).
//! - [`routes`]: serve-side ALPN -> local target routing + incoming handler.
//! - [`forwards`]: daemon-owned forward listeners.
//! - [`service`]: the `dev.iroh.Proxy` control interface implementation.
//! - [`control_plane`]: platform transport serving that interface.

mod connections;
mod control_plane;
mod forwards;
mod routes;
mod service;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::SecretKey;
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info, warn};

use crate::config::{ForwardService, ServeService};
use crate::forward::ForwardBinding;
use crate::proxy::build_endpoint;
use crate::remote_path::RemotePath;

use connections::ConnectionRegistry;
use forwards::{ForwardRuntime, add_forward_binding};
use routes::{Route, add_serve_route, handle_incoming, sync_endpoint_alpns};
use service::ProxyService;

/// Serve routes keyed by ALPN.
type Routes = Arc<RwLock<HashMap<Vec<u8>, Route>>>;
/// Daemon-owned forward listeners keyed by listen address.
type Forwards = Arc<Mutex<HashMap<Box<str>, ForwardRuntime>>>;

pub async fn run_server(
    secret_key: SecretKey,
    initial_services: Vec<ServeService>,
    initial_forwards: Vec<ForwardService>,
) -> Result<()> {
    let endpoint = build_endpoint(secret_key, true).await?;

    let routes: Routes = Arc::new(RwLock::new(HashMap::new()));
    let forwards: Forwards = Arc::new(Mutex::new(HashMap::new()));
    let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel::<Box<str>>();
    let connections = ConnectionRegistry::new(state_tx.clone());

    for service in initial_services {
        add_serve_route(&routes, &service.name, &service.target).await?;
    }
    sync_endpoint_alpns(&endpoint, &routes).await;

    for forward in initial_forwards {
        let remote = forward.remote.parse::<RemotePath>().with_context(|| {
            format!(
                "invalid remote path '{}' in persisted forward '{}'",
                forward.remote, forward.listen
            )
        })?;
        add_forward_binding(
            endpoint.clone(),
            Arc::clone(&forwards),
            connections.clone(),
            state_tx.clone(),
            ForwardBinding {
                listen: forward.listen,
                remote,
                close_on_request_timeout: Duration::from_secs(
                    forward.close_on_request_timeout_secs,
                ),
            },
            true,
        )
        .await?;
    }

    let svc = ProxyService {
        endpoint: endpoint.clone(),
        routes: Arc::clone(&routes),
        forwards: Arc::clone(&forwards),
        connections: connections.clone(),
        state_tx: state_tx.clone(),
    };

    let _control_plane = control_plane::start(svc, state_rx).await?;

    info!(endpoint_id = %endpoint.id(), "proxy server started");
    {
        let snapshot = routes.read().await;
        for (alpn, route) in snapshot.iter() {
            info!(
                endpoint = %endpoint.id(),
                alpn = %String::from_utf8_lossy(alpn),
                target = %route.target,
                "serving route"
            );
        }
    }

    let routes_for_accept = Arc::clone(&routes);
    let connections_for_accept = connections.clone();
    let mut accept_handle = tokio::spawn(async move {
        loop {
            let incoming = match endpoint.accept().await {
                Some(incoming) => incoming,
                None => {
                    warn!("endpoint closed");
                    return;
                }
            };

            let routes = Arc::clone(&routes_for_accept);
            let connections = connections_for_accept.clone();
            let peer_addr = incoming.remote_address();
            let local_ip = incoming.local_ip();
            tokio::spawn(async move {
                if let Err(err) = handle_incoming(incoming, Arc::clone(&routes), connections).await
                {
                    let available_services = {
                        let map = routes.read().await;
                        map.values()
                            .map(|route| route.name.as_ref())
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    error!(
                        peer_addr = %peer_addr,
                        local_ip = ?local_ip,
                        peer = "<unavailable: handshake failed>",
                        service = "<unavailable: handshake failed>",
                        available_services = %available_services,
                        error = %err,
                        "incoming connection error"
                    );
                }
            });
        }
    });

    tokio::select! {
        res = &mut accept_handle => {
            match res {
                Ok(()) => warn!("accept loop ended; iroh endpoint closed"),
                Err(err) => error!(error = %err, "accept loop task failed"),
            }
            control_plane::cleanup_socket();
            bail!("iroh endpoint closed; shutting down server");
        }
        _ = shutdown_signal() => {
            info!("shutdown signal received; stopping iroh-proxy server");
            accept_handle.abort();
            control_plane::cleanup_socket();
            Ok(())
        }
    }
}

/// Resolve when the process should shut down (SIGTERM/SIGINT on unix, Ctrl-C
/// elsewhere).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(err) => {
                warn!(error = %err, "failed to install SIGTERM handler");
                return std::future::pending().await;
            }
        };
        let mut interrupt = match signal(SignalKind::interrupt()) {
            Ok(sig) => sig,
            Err(err) => {
                warn!(error = %err, "failed to install SIGINT handler");
                return std::future::pending().await;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = interrupt.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
