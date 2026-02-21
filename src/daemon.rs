use std::collections::HashMap;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{
    Endpoint, RelayMode, SecretKey,
    address_lookup::{DhtAddressLookup, MdnsAddressLookup},
};
use tokio::io;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock, mpsc::UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use zbus::ConnectionBuilder;
use zbus::fdo;

use crate::config::{ForwardService, ServeService};
use crate::control::{BUS_NAME, INTERFACE, OBJECT_PATH};
use crate::forward::ForwardBinding;
use crate::remote_path::{RemotePath, service_to_alpn};

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
    let _dbus = ConnectionBuilder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, svc)?
        .build()
        .await?;
    let dbus_signal_conn = _dbus.clone();
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
