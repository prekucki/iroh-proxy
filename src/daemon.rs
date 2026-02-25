use std::collections::HashMap;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::io::ErrorKind;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use futures_util::StreamExt;
use iroh::{
    Endpoint, RelayMode, SecretKey,
    address_lookup::{DhtAddressLookup, MdnsAddressLookup},
};
use tokio::io;
use tokio::net::{TcpListener, TcpStream};
#[cfg(target_os = "macos")]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, RwLock, mpsc::UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use zbus::Connection;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use zbus::MessageStream;
use zbus::connection::Builder as ConnectionBuilder;
use zbus::fdo;

use crate::config::{ForwardService, ServeService};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::control::p2p_control_socket_path;
use crate::control::{BUS_NAME, INTERFACE, OBJECT_PATH};
use crate::forward::ForwardBinding;
use crate::remote_path::{RemotePath, service_to_alpn};
#[cfg(target_os = "windows")]
use uds_windows::{UnixListener as WindowsUnixListener, UnixStream as WindowsUnixStream};

#[derive(Debug, Clone)]
struct Route {
    name: Box<str>,
    target: Box<str>,
}

#[derive(Debug)]
struct ForwardRuntime {
    remote: RemotePath,
    persisted: bool,
    task: JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct ActiveConnection {
    src: Box<str>,
    kind: Box<str>,
    dst: Box<str>,
}

struct ActiveConnGuard {
    id: u64,
    active_connections: Arc<StdMutex<HashMap<u64, ActiveConnection>>>,
    state_tx: UnboundedSender<Box<str>>,
}

impl Drop for ActiveConnGuard {
    fn drop(&mut self) {
        let mut map = self
            .active_connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.remove(&self.id);
        let _ = self.state_tx.send("connection-closed".into());
    }
}

#[derive(Clone)]
struct ProxyService {
    endpoint: Endpoint,
    routes: Arc<RwLock<HashMap<Vec<u8>, Route>>>,
    forwards: Arc<Mutex<HashMap<Box<str>, ForwardRuntime>>>,
    active_connections: Arc<StdMutex<HashMap<u64, ActiveConnection>>>,
    next_conn_id: Arc<AtomicU64>,
    state_tx: UnboundedSender<Box<str>>,
}

#[zbus::interface(name = "dev.iroh.Proxy")]
impl ProxyService {
    #[zbus(name = "Status")]
    async fn status(&self) -> fdo::Result<(String, u64, u64, u64)> {
        let served = self.routes.read().await.len() as u64;
        let forwards = self.forwards.lock().await.len() as u64;
        let connections = self
            .active_connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len() as u64;
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
            .active_connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(id, conn)| {
                (
                    *id,
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
    async fn add_forward(&self, listen: &str, remote: &str, persisted: bool) -> fdo::Result<()> {
        add_forward_binding(
            self.endpoint.clone(),
            Arc::clone(&self.forwards),
            Arc::clone(&self.active_connections),
            Arc::clone(&self.next_conn_id),
            self.state_tx.clone(),
            ForwardBinding {
                listen: listen.into(),
                remote: remote.parse().map_err(to_fdo)?,
            },
            persisted,
        )
        .await
        .map_err(to_fdo)?;
        info!(listen, remote, "added forward binding");
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

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn is_expected_signal_disconnect(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::InputOutput(ioerr) => matches!(
            ioerr.kind(),
            ErrorKind::BrokenPipe
                | ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionReset
                | ErrorKind::NotConnected
                | ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn spawn_p2p_state_signal_fanout(
    mut state_rx: tokio::sync::mpsc::UnboundedReceiver<Box<str>>,
    peers: Arc<Mutex<HashMap<u64, Connection>>>,
) {
    tokio::spawn(async move {
        while let Some(reason) = state_rx.recv().await {
            let snapshot = { peers.lock().await.clone() };
            if snapshot.is_empty() {
                continue;
            }

            let mut failed_ids = Vec::new();
            for (peer_id, conn) in snapshot {
                if let Err(err) = conn
                    .emit_signal(
                        None::<&str>,
                        OBJECT_PATH,
                        INTERFACE,
                        "StateChanged",
                        &(reason.as_ref(),),
                    )
                    .await
                {
                    if is_expected_signal_disconnect(&err) {
                        info!(error = %err, reason = %reason, "control peer disconnected");
                    } else {
                        warn!(error = %err, reason = %reason, "failed to emit state change signal");
                    }
                    failed_ids.push(peer_id);
                }
            }

            if !failed_ids.is_empty() {
                let mut map = peers.lock().await;
                for peer_id in failed_ids {
                    map.remove(&peer_id);
                }
            }
        }
    });
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn spawn_p2p_peer_disconnect_monitor(
    peer_id: u64,
    conn: Connection,
    peers: Arc<Mutex<HashMap<u64, Connection>>>,
) {
    tokio::spawn(async move {
        let mut stream = MessageStream::from(conn);
        while let Some(message) = stream.next().await {
            if let Err(err) = message {
                if is_expected_signal_disconnect(&err) {
                    info!(peer_id, error = %err, "control peer disconnected");
                } else {
                    warn!(peer_id, error = %err, "control peer stream ended with error");
                }
                break;
            }
        }

        peers.lock().await.remove(&peer_id);
    });
}

#[cfg(target_os = "macos")]
async fn bind_macos_control_socket(socket_path: &std::path::Path) -> Result<UnixListener> {
    match UnixListener::bind(socket_path) {
        Ok(listener) => Ok(listener),
        Err(bind_err) if bind_err.kind() == ErrorKind::AddrInUse => {
            match UnixStream::connect(socket_path).await {
                Ok(_) => bail!(
                    "control socket {} is already served by a running daemon",
                    socket_path.display()
                ),
                Err(connect_err)
                    if matches!(
                        connect_err.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    ) =>
                {
                    match std::fs::remove_file(socket_path) {
                        Ok(()) => {}
                        Err(remove_err) if remove_err.kind() == ErrorKind::NotFound => {}
                        Err(remove_err) => {
                            return Err(remove_err).with_context(|| {
                                format!(
                                    "failed to remove stale control socket {}",
                                    socket_path.display()
                                )
                            });
                        }
                    }
                    UnixListener::bind(socket_path).with_context(|| {
                        format!(
                            "failed to bind control socket {} after stale cleanup",
                            socket_path.display()
                        )
                    })
                }
                Err(connect_err) => Err(connect_err).with_context(|| {
                    format!(
                        "failed to probe existing control socket {}",
                        socket_path.display()
                    )
                }),
            }
        }
        Err(bind_err) => Err(bind_err)
            .with_context(|| format!("failed to bind control socket {}", socket_path.display())),
    }
}

#[cfg(target_os = "windows")]
fn bind_windows_control_socket(socket_path: &std::path::Path) -> Result<WindowsUnixListener> {
    match WindowsUnixListener::bind(socket_path) {
        Ok(listener) => Ok(listener),
        Err(bind_err) if bind_err.kind() == ErrorKind::AddrInUse => {
            match WindowsUnixStream::connect(socket_path) {
                Ok(_) => bail!(
                    "control socket {} is already served by a running daemon",
                    socket_path.display()
                ),
                Err(connect_err)
                    if matches!(
                        connect_err.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    ) =>
                {
                    match std::fs::remove_file(socket_path) {
                        Ok(()) => {}
                        Err(remove_err) if remove_err.kind() == ErrorKind::NotFound => {}
                        Err(remove_err) => {
                            return Err(remove_err).with_context(|| {
                                format!(
                                    "failed to remove stale control socket {}",
                                    socket_path.display()
                                )
                            });
                        }
                    }
                    WindowsUnixListener::bind(socket_path).with_context(|| {
                        format!(
                            "failed to bind control socket {} after stale cleanup",
                            socket_path.display()
                        )
                    })
                }
                Err(connect_err) => Err(connect_err).with_context(|| {
                    format!(
                        "failed to probe existing control socket {}",
                        socket_path.display()
                    )
                }),
            }
        }
        Err(bind_err) => Err(bind_err)
            .with_context(|| format!("failed to bind control socket {}", socket_path.display())),
    }
}

#[cfg(target_os = "macos")]
async fn start_p2p_control_plane(
    svc: ProxyService,
    state_rx: tokio::sync::mpsc::UnboundedReceiver<Box<str>>,
) -> Result<()> {
    let socket_path = p2p_control_socket_path();
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create control socket directory {}",
                parent.display()
            )
        })?;
    }
    let listener = bind_macos_control_socket(&socket_path).await?;
    let peers = Arc::new(Mutex::new(HashMap::<u64, Connection>::new()));
    let next_peer_id = Arc::new(AtomicU64::new(1));
    spawn_p2p_state_signal_fanout(state_rx, Arc::clone(&peers));

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(err) => {
                    warn!(error = %err, "control socket accept failed");
                    continue;
                }
            };

            let builder = ConnectionBuilder::unix_stream(stream)
                .server(zbus::Guid::generate())
                .and_then(|builder| builder.p2p().serve_at(OBJECT_PATH, svc.clone()))
                .and_then(|builder| builder.name(BUS_NAME));
            let builder = match builder {
                Ok(builder) => builder,
                Err(err) => {
                    warn!(error = %err, "failed preparing p2p control connection");
                    continue;
                }
            };

            match builder.build().await {
                Ok(conn) => {
                    let peer_id = next_peer_id.fetch_add(1, Ordering::Relaxed);
                    peers.lock().await.insert(peer_id, conn.clone());
                    spawn_p2p_peer_disconnect_monitor(peer_id, conn, Arc::clone(&peers));
                }
                Err(err) => warn!(error = %err, "failed creating p2p control connection"),
            }
        }
    });

    info!(socket = %socket_path.display(), "p2p control plane ready");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn start_p2p_control_plane(
    svc: ProxyService,
    state_rx: tokio::sync::mpsc::UnboundedReceiver<Box<str>>,
) -> Result<()> {
    let socket_path = p2p_control_socket_path();
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create control socket directory {}",
                parent.display()
            )
        })?;
    }
    let listener = bind_windows_control_socket(&socket_path)?;
    let peers = Arc::new(Mutex::new(HashMap::<u64, Connection>::new()));
    let next_peer_id = Arc::new(AtomicU64::new(1));
    spawn_p2p_state_signal_fanout(state_rx, Arc::clone(&peers));

    tokio::spawn(async move {
        loop {
            let accept_listener = match listener.try_clone() {
                Ok(listener) => listener,
                Err(err) => {
                    warn!(error = %err, "failed cloning control socket listener");
                    break;
                }
            };
            let (stream, _) =
                match tokio::task::spawn_blocking(move || accept_listener.accept()).await {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(err)) => {
                        warn!(error = %err, "control socket accept failed");
                        continue;
                    }
                    Err(err) => {
                        warn!(error = %err, "control socket accept task failed");
                        continue;
                    }
                };

            let builder = ConnectionBuilder::unix_stream(stream)
                .server(zbus::Guid::generate())
                .map(|builder| builder.p2p())
                .and_then(|builder| builder.serve_at(OBJECT_PATH, svc.clone()))
                .and_then(|builder| builder.name(BUS_NAME));
            let builder = match builder {
                Ok(builder) => builder,
                Err(err) => {
                    warn!(error = %err, "failed preparing p2p control connection");
                    continue;
                }
            };

            match builder.build().await {
                Ok(conn) => {
                    let peer_id = next_peer_id.fetch_add(1, Ordering::Relaxed);
                    peers.lock().await.insert(peer_id, conn.clone());
                    spawn_p2p_peer_disconnect_monitor(peer_id, conn, Arc::clone(&peers));
                }
                Err(err) => warn!(error = %err, "failed creating p2p control connection"),
            }
        }
    });

    info!(socket = %socket_path.display(), "p2p control plane ready");
    Ok(())
}

async fn sync_endpoint_alpns(endpoint: &Endpoint, routes: &Arc<RwLock<HashMap<Vec<u8>, Route>>>) {
    let alpns = {
        let map = routes.read().await;
        map.keys().cloned().collect::<Vec<_>>()
    };
    endpoint.set_alpns(alpns);
}

pub async fn run_server(
    secret_key: SecretKey,
    initial_services: Vec<ServeService>,
    initial_forwards: Vec<ForwardService>,
) -> Result<()> {
    let endpoint = Endpoint::empty_builder(RelayMode::Default)
        .secret_key(secret_key)
        .address_lookup(DhtAddressLookup::builder().n0_dns_pkarr_relay())
        .address_lookup(MdnsAddressLookup::builder())
        .bind()
        .await?;
    endpoint.online().await;

    let routes = Arc::new(RwLock::new(HashMap::<Vec<u8>, Route>::new()));
    let forwards = Arc::new(Mutex::new(HashMap::<Box<str>, ForwardRuntime>::new()));
    let active_connections = Arc::new(StdMutex::new(HashMap::<u64, ActiveConnection>::new()));
    let next_conn_id = Arc::new(AtomicU64::new(1));
    #[cfg_attr(any(target_os = "macos", target_os = "windows"), allow(unused_mut))]
    let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel::<Box<str>>();

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
            Arc::clone(&active_connections),
            Arc::clone(&next_conn_id),
            state_tx.clone(),
            ForwardBinding {
                listen: forward.listen,
                remote,
            },
            true,
        )
        .await?;
    }

    let svc = ProxyService {
        endpoint: endpoint.clone(),
        routes: Arc::clone(&routes),
        forwards: Arc::clone(&forwards),
        active_connections: Arc::clone(&active_connections),
        next_conn_id: Arc::clone(&next_conn_id),
        state_tx: state_tx.clone(),
    };

    #[cfg(target_os = "linux")]
    let _control_plane = {
        let dbus = ConnectionBuilder::session()?
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, svc)?
            .build()
            .await?;
        let dbus_signal_conn = dbus.clone();
        tokio::spawn(async move {
            while let Some(reason) = state_rx.recv().await {
                if let Err(err) = dbus_signal_conn
                    .emit_signal(
                        None::<&str>,
                        OBJECT_PATH,
                        INTERFACE,
                        "StateChanged",
                        &(reason.as_ref(),),
                    )
                    .await
                {
                    warn!(error = %err, reason = %reason, "failed to emit state change signal");
                }
            }
        });
        dbus
    };

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    start_p2p_control_plane(svc, state_rx).await?;

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    tokio::spawn(async move {
        warn!(
            os = %std::env::consts::OS,
            "control API transport is not implemented yet on this platform"
        );
        while state_rx.recv().await.is_some() {}
    });

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
    let active_for_accept = Arc::clone(&active_connections);
    let next_conn_id_for_accept = Arc::clone(&next_conn_id);
    let state_tx_for_accept = state_tx.clone();
    tokio::spawn(async move {
        loop {
            let incoming = match endpoint.accept().await {
                Some(incoming) => incoming,
                None => {
                    warn!("endpoint closed");
                    return;
                }
            };

            let routes = Arc::clone(&routes_for_accept);
            let active_connections = Arc::clone(&active_for_accept);
            let next_conn_id = Arc::clone(&next_conn_id_for_accept);
            let state_tx = state_tx_for_accept.clone();
            let peer_addr = incoming.remote_address();
            let local_ip = incoming.local_ip();
            tokio::spawn(async move {
                if let Err(err) = handle_incoming(
                    incoming,
                    Arc::clone(&routes),
                    active_connections,
                    next_conn_id,
                    state_tx,
                )
                .await
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

    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}

async fn add_serve_route(
    routes: &Arc<RwLock<HashMap<Vec<u8>, Route>>>,
    name: &str,
    target: &str,
) -> Result<()> {
    if name.trim().is_empty() {
        bail!("service name cannot be empty");
    }
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

async fn add_forward_binding(
    endpoint: Endpoint,
    forwards: Arc<Mutex<HashMap<Box<str>, ForwardRuntime>>>,
    active_connections: Arc<StdMutex<HashMap<u64, ActiveConnection>>>,
    next_conn_id: Arc<AtomicU64>,
    state_tx: UnboundedSender<Box<str>>,
    binding: ForwardBinding,
    persisted: bool,
) -> Result<()> {
    let mut map = forwards.lock().await;
    if map.contains_key(&binding.listen) {
        bail!("listener {} already exists", binding.listen);
    }

    let listener = TcpListener::bind(&*binding.listen)
        .await
        .with_context(|| format!("failed to bind local listener {}", binding.listen))?;
    let listen = binding.listen.clone();
    let remote = binding.remote.clone();
    let alpn = remote.to_alpn();
    let state_tx_for_task = state_tx.clone();

    let task = tokio::spawn(async move {
        loop {
            let (inbound, peer_addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(err) => {
                    error!(error = %err, listen = %listen, "forward listener accept failed");
                    return;
                }
            };

            let endpoint = endpoint.clone();
            let remote = remote.clone();
            let alpn = alpn.clone();
            let active_connections = Arc::clone(&active_connections);
            let next_conn_id = Arc::clone(&next_conn_id);
            let state_tx = state_tx_for_task.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_forward_conn(
                    endpoint,
                    inbound,
                    remote,
                    alpn,
                    active_connections,
                    next_conn_id,
                    state_tx,
                )
                .await
                {
                    warn!(peer = %peer_addr, error = %err, "forwarding connection failed");
                }
            });
        }
    });

    map.insert(
        binding.listen.clone(),
        ForwardRuntime {
            remote: binding.remote,
            persisted,
            task,
        },
    );
    let _ = state_tx.send("forward-added".into());

    Ok(())
}

async fn handle_incoming(
    incoming: iroh::endpoint::Incoming,
    routes: Arc<RwLock<HashMap<Vec<u8>, Route>>>,
    active_connections: Arc<StdMutex<HashMap<u64, ActiveConnection>>>,
    next_conn_id: Arc<AtomicU64>,
    state_tx: UnboundedSender<Box<str>>,
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
    warn!(
        peer = %conn.remote_id(),
        service = %route.name,
        "accepted incoming connection"
    );

    let _guard = register_connection(
        Arc::clone(&active_connections),
        Arc::clone(&next_conn_id),
        state_tx.clone(),
        ActiveConnection {
            src: conn.remote_id().to_string().into(),
            kind: "serve".into(),
            dst: route.name.clone(),
        },
    );
    let (mut send, mut recv) = conn.accept_bi().await?;
    let local = TcpStream::connect(&*route.target)
        .await
        .with_context(|| format!("failed to connect local target {}", route.target))?;
    let (mut local_read, mut local_write) = local.into_split();

    let to_local = io::copy(&mut recv, &mut local_write);
    let to_remote = io::copy(&mut local_read, &mut send);
    let _ = tokio::try_join!(to_local, to_remote)?;
    send.finish()?;
    Ok(())
}

async fn handle_forward_conn(
    endpoint: Endpoint,
    inbound: TcpStream,
    remote: RemotePath,
    alpn: Vec<u8>,
    active_connections: Arc<StdMutex<HashMap<u64, ActiveConnection>>>,
    next_conn_id: Arc<AtomicU64>,
    state_tx: UnboundedSender<Box<str>>,
) -> Result<()> {
    let src = inbound
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let conn = endpoint
        .connect(remote.endpoint_id, &alpn)
        .await
        .with_context(|| {
            format!(
                "failed connecting to remote endpoint {} (discovery failed via local mDNS and pkarr)",
                remote.endpoint_id
            )
        })?;
    let _guard = register_connection(
        Arc::clone(&active_connections),
        Arc::clone(&next_conn_id),
        state_tx.clone(),
        ActiveConnection {
            src: src.into(),
            kind: "forward".into(),
            dst: format!("{}/tcp/{}", remote.endpoint_id, remote.service).into(),
        },
    );
    let (mut send, mut recv) = conn.open_bi().await?;
    let (mut inbound_read, mut inbound_write) = inbound.into_split();

    let to_remote = io::copy(&mut inbound_read, &mut send);
    let to_local = io::copy(&mut recv, &mut inbound_write);
    let _ = tokio::try_join!(to_remote, to_local)?;
    send.finish()?;
    Ok(())
}

fn register_connection(
    active_connections: Arc<StdMutex<HashMap<u64, ActiveConnection>>>,
    next_conn_id: Arc<AtomicU64>,
    state_tx: UnboundedSender<Box<str>>,
    info: ActiveConnection,
) -> ActiveConnGuard {
    let id = next_conn_id.fetch_add(1, Ordering::Relaxed);
    {
        let mut map = active_connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert(id, info);
    }
    let _ = state_tx.send("connection-opened".into());
    ActiveConnGuard {
        id,
        active_connections: Arc::clone(&active_connections),
        state_tx,
    }
}

impl Drop for ForwardRuntime {
    fn drop(&mut self) {
        self.task.abort();
    }
}
