use anyhow::{Context, Result, anyhow};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use tokio::net::UnixStream;
use tracing::{debug, warn};
#[cfg(target_os = "windows")]
use uds_windows::UnixStream as WindowsUnixStream;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use zbus::connection::Builder as ConnectionBuilder;
use zbus::{Connection, Proxy};

pub const BUS_NAME: &str = "dev.iroh.Proxy";
pub const OBJECT_PATH: &str = "/dev/iroh/Proxy";
pub const INTERFACE: &str = "dev.iroh.Proxy";

#[derive(Clone)]
pub struct ControlClient {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub endpoint_id: Box<str>,
    pub connections: u64,
    pub served: u64,
    pub forwards: u64,
}

#[derive(Debug, Clone)]
pub struct ActiveConnection {
    pub id: u64,
    pub src: Box<str>,
    pub kind: Box<str>,
    pub dst: Box<str>,
}

#[derive(Debug, Clone)]
pub struct ServeRoute {
    pub name: Box<str>,
    pub target: Box<str>,
}

#[derive(Debug, Clone)]
pub struct ForwardRoute {
    pub listen: Box<str>,
    pub remote: Box<str>,
    pub persisted: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub live_control: bool,
    pub state_stream: bool,
    pub transport_label: &'static str,
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        live_control: true,
        state_stream: true,
        transport_label: control_transport_label(),
    }
}

#[cfg(target_os = "linux")]
fn control_transport_label() -> &'static str {
    "dbus"
}

#[cfg(target_os = "macos")]
fn control_transport_label() -> &'static str {
    "zbus-p2p-uds"
}

#[cfg(target_os = "windows")]
fn control_transport_label() -> &'static str {
    "zbus-p2p-windows-uds"
}

#[cfg(target_os = "macos")]
pub fn p2p_control_socket_path() -> PathBuf {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("iroh-proxy").join("control.sock")
}

#[cfg(target_os = "windows")]
pub fn p2p_control_socket_path() -> PathBuf {
    std::env::temp_dir().join("iroh-proxy").join("control.sock")
}

#[cfg(target_os = "linux")]
async fn platform_connection() -> Result<Connection> {
    Connection::session()
        .await
        .context("failed to connect to DBus session bus")
}

#[cfg(target_os = "macos")]
async fn platform_connection() -> Result<Connection> {
    let socket_path = p2p_control_socket_path();
    let stream = UnixStream::connect(&socket_path)
        .await
        .with_context(|| format!("failed to connect control socket {}", socket_path.display()))?;
    ConnectionBuilder::unix_stream(stream)
        .p2p()
        .build()
        .await
        .context("failed to establish zbus p2p connection over unix socket")
}

#[cfg(target_os = "windows")]
async fn platform_connection() -> Result<Connection> {
    let socket_path = p2p_control_socket_path();
    let stream = tokio::task::spawn_blocking({
        let socket_path = socket_path.clone();
        move || WindowsUnixStream::connect(&socket_path)
    })
    .await
    .context("failed to join windows uds connect task")?
    .with_context(|| format!("failed to connect control socket {}", socket_path.display()))?;
    ConnectionBuilder::unix_stream(stream)
        .p2p()
        .build()
        .await
        .context("failed to establish zbus p2p connection over windows uds")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("iroh-proxy supports Linux, macOS, and Windows only");

async fn connect_control() -> Result<Connection> {
    platform_connection().await
}

impl ControlClient {
    pub async fn connect() -> Result<Self> {
        let conn = connect_control().await?;
        Ok(Self { conn })
    }

    async fn proxy(&self) -> Result<Proxy<'_>> {
        Proxy::new(&self.conn, BUS_NAME, OBJECT_PATH, INTERFACE)
            .await
            .context("failed to connect to iroh-proxy control interface")
    }

    pub async fn status(&self) -> Result<Option<Status>> {
        let proxy = match self.proxy().await {
            Ok(proxy) => proxy,
            Err(err) => {
                // The transport connected but the interface proxy could not be
                // built — unusual, so surface it rather than masking it.
                warn!(error = %err, "failed to build control proxy; treating server as stopped");
                return Ok(None);
            }
        };

        let call: Result<(String, u64, u64, u64), zbus::Error> = proxy.call("Status", &()).await;
        match call {
            Ok((endpoint_id, connections, served, forwards)) => Ok(Some(Status {
                endpoint_id: endpoint_id.into(),
                connections,
                served,
                forwards,
            })),
            Err(err) => {
                // Commonly "service not running"; log at debug so a broken-but-
                // present daemon is still diagnosable with RUST_LOG=debug.
                debug!(error = %err, "control Status call failed; treating server as stopped");
                Ok(None)
            }
        }
    }

    pub async fn add_forward(
        &self,
        listen: &str,
        remote: &str,
        persisted: bool,
        close_on_request_timeout_secs: u64,
    ) -> Result<()> {
        let proxy = self.proxy().await?;
        let _: () = proxy
            .call(
                "AddForward",
                &(listen, remote, persisted, close_on_request_timeout_secs),
            )
            .await
            .map_err(|err| anyhow!("AddForward failed: {err}"))?;
        Ok(())
    }

    pub async fn list_connections(&self) -> Result<Vec<ActiveConnection>> {
        let proxy = self.proxy().await?;

        let rows: Vec<(u64, String, String, String)> = proxy
            .call("ListConnections", &())
            .await
            .map_err(|err| anyhow!("ListConnections failed: {err}"))?;
        Ok(rows
            .into_iter()
            .map(|(id, src, kind, dst)| ActiveConnection {
                id,
                src: src.into(),
                kind: kind.into(),
                dst: dst.into(),
            })
            .collect())
    }

    pub async fn list_serves(&self) -> Result<Vec<ServeRoute>> {
        let proxy = self.proxy().await?;

        let rows: Vec<(String, String)> = proxy
            .call("ListServes", &())
            .await
            .map_err(|err| anyhow!("ListServes failed: {err}"))?;
        Ok(rows
            .into_iter()
            .map(|(name, target)| ServeRoute {
                name: name.into(),
                target: target.into(),
            })
            .collect())
    }

    pub async fn list_forwards(&self) -> Result<Vec<ForwardRoute>> {
        let proxy = self.proxy().await?;

        let rows: Vec<(String, String, bool)> = proxy
            .call("ListForwards", &())
            .await
            .map_err(|err| anyhow!("ListForwards failed: {err}"))?;
        Ok(rows
            .into_iter()
            .map(|(listen, remote, persisted)| ForwardRoute {
                listen: listen.into(),
                remote: remote.into(),
                persisted,
            })
            .collect())
    }

    pub async fn del_forward(&self, listen: &str) -> Result<()> {
        let proxy = self.proxy().await?;
        let _: () = proxy
            .call("DelForward", &(listen,))
            .await
            .map_err(|err| anyhow!("DelForward failed: {err}"))?;
        Ok(())
    }

    pub async fn add_serve(&self, name: &str, target: &str) -> Result<()> {
        let proxy = self.proxy().await?;
        let _: () = proxy
            .call("AddServe", &(name, target))
            .await
            .map_err(|err| anyhow!("AddServe failed: {err}"))?;
        Ok(())
    }

    pub async fn del_serve(&self, name: &str) -> Result<()> {
        let proxy = self.proxy().await?;
        let _: () = proxy
            .call("DelServe", &(name,))
            .await
            .map_err(|err| anyhow!("DelServe failed: {err}"))?;
        Ok(())
    }
}

/// Connect to the control plane and confirm a daemon is answering.
///
/// `Ok(None)` covers the common "server not running" cases (transport connect
/// failure, Status call failure) — those are logged at debug so a broken-but-
/// present daemon stays diagnosable with `RUST_LOG=debug`. The returned client
/// should be reused for every RPC of the current command instead of opening a
/// fresh connection per call.
pub async fn connect_running() -> Result<Option<(ControlClient, Status)>> {
    let client = match ControlClient::connect().await {
        Ok(client) => client,
        Err(err) => {
            debug!(error = %err, "failed to connect to control plane; treating server as stopped");
            return Ok(None);
        }
    };
    match client.status().await? {
        Some(status) => Ok(Some((client, status))),
        None => Ok(None),
    }
}
