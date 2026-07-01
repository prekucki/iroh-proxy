//! The `dev.iroh.Proxy` control interface served over the platform control
//! plane (DBus on Linux, zbus p2p on macOS/Windows).

use std::sync::Arc;
use std::time::Duration;

use iroh::Endpoint;
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;
use zbus::fdo;

use crate::forward::ForwardBinding;
use crate::remote_path::service_to_alpn;

use super::connections::ConnectionRegistry;
use super::forwards::add_forward_binding;
use super::routes::{add_serve_route, sync_endpoint_alpns};
use super::{Forwards, Routes};

#[derive(Clone)]
pub(super) struct ProxyService {
    pub(super) endpoint: Endpoint,
    pub(super) routes: Routes,
    pub(super) forwards: Forwards,
    pub(super) connections: ConnectionRegistry,
    pub(super) state_tx: UnboundedSender<Box<str>>,
}

#[zbus::interface(name = "dev.iroh.Proxy")]
impl ProxyService {
    #[zbus(name = "Status")]
    async fn status(&self) -> fdo::Result<(String, u64, u64, u64)> {
        let served = self.routes.read().await.len() as u64;
        let forwards = self.forwards.lock().await.len() as u64;
        let connections = self.connections.len() as u64;
        Ok((
            self.endpoint.id().to_string(),
            connections,
            served,
            forwards,
        ))
    }

    #[zbus(name = "ListConnections")]
    async fn list_connections(&self) -> fdo::Result<Vec<(u64, String, String, String)>> {
        let rows = self
            .connections
            .snapshot()
            .into_iter()
            .map(|(id, conn)| {
                (
                    id,
                    conn.src.to_string(),
                    conn.kind.to_string(),
                    conn.dst.to_string(),
                )
            })
            .collect::<Vec<_>>();
        Ok(rows)
    }

    #[zbus(name = "ListServes")]
    async fn list_serves(&self) -> fdo::Result<Vec<(String, String)>> {
        let rows = self
            .routes
            .read()
            .await
            .values()
            .map(|route| (route.name.to_string(), route.target.to_string()))
            .collect::<Vec<_>>();
        Ok(rows)
    }

    #[zbus(name = "ListForwards")]
    async fn list_forwards(&self) -> fdo::Result<Vec<(String, String, bool)>> {
        let rows = self
            .forwards
            .lock()
            .await
            .iter()
            .map(|(listen, runtime)| {
                (
                    listen.to_string(),
                    format!(
                        "{}/tcp/{}",
                        runtime.remote.endpoint_id, runtime.remote.service
                    ),
                    runtime.persisted,
                )
            })
            .collect::<Vec<_>>();
        Ok(rows)
    }

    #[zbus(name = "AddServe")]
    async fn add_serve(&self, name: &str, target: &str) -> fdo::Result<()> {
        add_serve_route(&self.routes, name, target)
            .await
            .map_err(to_fdo)?;
        sync_endpoint_alpns(&self.endpoint, &self.routes).await;
        let _ = self.state_tx.send("serve-added".into());
        info!(service = name, target, "added serve route");
        Ok(())
    }

    #[zbus(name = "DelServe")]
    async fn del_serve(&self, name: &str) -> fdo::Result<()> {
        let alpn = service_to_alpn(name);
        let removed = self.routes.write().await.remove(&alpn);
        if removed.is_none() {
            return Err(fdo::Error::Failed(format!(
                "service '{}' is not currently served",
                name
            )));
        }
        sync_endpoint_alpns(&self.endpoint, &self.routes).await;
        let _ = self.state_tx.send("serve-removed".into());
        info!(service = name, "removed serve route");
        Ok(())
    }

    #[zbus(name = "AddForward")]
    async fn add_forward(
        &self,
        listen: &str,
        remote: &str,
        persisted: bool,
        close_on_request_timeout_secs: u64,
    ) -> fdo::Result<()> {
        add_forward_binding(
            self.endpoint.clone(),
            Arc::clone(&self.forwards),
            self.connections.clone(),
            self.state_tx.clone(),
            ForwardBinding {
                listen: listen.into(),
                remote: remote.parse().map_err(to_fdo)?,
                close_on_request_timeout: Duration::from_secs(close_on_request_timeout_secs),
            },
            persisted,
        )
        .await
        .map_err(to_fdo)?;
        info!(
            listen,
            remote, close_on_request_timeout_secs, "added forward binding"
        );
        Ok(())
    }

    #[zbus(name = "DelForward")]
    async fn del_forward(&self, listen: &str) -> fdo::Result<()> {
        let removed = self.forwards.lock().await.remove(listen);
        if removed.is_none() {
            return Err(fdo::Error::Failed(format!(
                "listener '{}' is not currently forwarded",
                listen
            )));
        }
        let _ = self.state_tx.send("forward-removed".into());
        info!(listen, "removed forward binding");
        Ok(())
    }
}

fn to_fdo(err: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(err.to_string())
}
