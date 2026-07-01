//! Platform transport serving the control interface: the DBus session bus on
//! Linux, a zbus p2p connection over a unix(-like) socket on macOS/Windows.
//!
//! The macOS and Windows transports differ only in socket primitives; the
//! stale-socket rebind state machine ([`bind_control_listener`]) and the accept
//! loop ([`start_p2p_control_plane`]) are written once over [`ControlListener`].

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::collections::HashMap;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::io::ErrorKind;
#[cfg(target_os = "macos")]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::time::Duration;

use anyhow::Result;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use anyhow::{Context, bail};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use futures_util::StreamExt;
#[cfg(target_os = "macos")]
use tokio::net::{UnixListener, UnixStream};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tracing::info;
use tracing::warn;
#[cfg(target_os = "windows")]
use uds_windows::{UnixListener as WindowsUnixListener, UnixStream as WindowsUnixStream};
use zbus::Connection;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use zbus::MessageStream;
use zbus::connection::Builder as ConnectionBuilder;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::control::p2p_control_socket_path;
use crate::control::{BUS_NAME, INTERFACE, OBJECT_PATH};

use super::service::ProxyService;

/// Keeps the control-plane transport alive; `run_server` holds it for the
/// server's lifetime.
pub(super) struct ControlPlaneHandle {
    #[cfg(target_os = "linux")]
    _dbus: Connection,
}

#[cfg(target_os = "linux")]
pub(super) async fn start(
    svc: ProxyService,
    mut state_rx: UnboundedReceiver<Box<str>>,
) -> Result<ControlPlaneHandle> {
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
    Ok(ControlPlaneHandle { _dbus: dbus })
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(super) async fn start(
    svc: ProxyService,
    state_rx: UnboundedReceiver<Box<str>>,
) -> Result<ControlPlaneHandle> {
    #[cfg(target_os = "macos")]
    type PlatformListener = MacosControlListener;
    #[cfg(target_os = "windows")]
    type PlatformListener = WindowsControlListener;

    start_p2p_control_plane::<PlatformListener>(svc, state_rx).await?;
    Ok(ControlPlaneHandle {})
}

/// Remove the p2p control socket on shutdown (macOS/Windows). No-op on Linux,
/// which uses the DBus session bus rather than a socket file.
pub(super) fn cleanup_socket() {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let path = p2p_control_socket_path();
        match std::fs::remove_file(&path) {
            Ok(()) => info!(socket = %path.display(), "removed control socket"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(
                error = %err,
                socket = %path.display(),
                "failed to remove control socket on shutdown"
            ),
        }
    }
}

/// Platform control-socket listener. Only the socket primitives differ per
/// platform; binding, probing, accepting and hardening policy are driven by
/// the shared code above this trait.
///
/// Futures are declared `+ Send` explicitly because the accept loop runs the
/// listener inside a spawned task.
#[cfg(any(target_os = "macos", target_os = "windows"))]
trait ControlListener: Sized + Send + 'static {
    /// Stream type accepted by [`ConnectionBuilder::unix_stream`] on this
    /// platform.
    type Stream: Send + 'static;

    fn bind(path: &Path) -> impl Future<Output = std::io::Result<Self>> + Send;
    /// Probe whether a live daemon is serving `path`.
    fn probe(path: &Path) -> impl Future<Output = std::io::Result<()>> + Send;
    fn accept(&self) -> impl Future<Output = std::io::Result<Self::Stream>> + Send;
    /// Reject peers that must not drive the daemon (e.g. other local users).
    fn authorize(&self, stream: &Self::Stream) -> std::io::Result<()>;
    fn stream_builder(stream: Self::Stream) -> ConnectionBuilder<'static>;
}

#[cfg(target_os = "macos")]
struct MacosControlListener {
    listener: UnixListener,
    /// Uid owning the socket file (the daemon's euid); the control interface
    /// has no other authentication, so only this uid may connect.
    owner_uid: u32,
}

#[cfg(target_os = "macos")]
impl ControlListener for MacosControlListener {
    type Stream = UnixStream;

    async fn bind(path: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        let owner_uid = std::fs::metadata(path)?.uid();
        Ok(Self {
            listener,
            owner_uid,
        })
    }

    async fn probe(path: &Path) -> std::io::Result<()> {
        UnixStream::connect(path).await.map(|_| ())
    }

    async fn accept(&self) -> std::io::Result<UnixStream> {
        self.listener.accept().await.map(|(stream, _)| stream)
    }

    fn authorize(&self, stream: &UnixStream) -> std::io::Result<()> {
        let peer_uid = stream.peer_cred()?.uid();
        if peer_uid != self.owner_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "peer uid {peer_uid} does not match daemon uid {}",
                    self.owner_uid
                ),
            ));
        }
        Ok(())
    }

    fn stream_builder(stream: UnixStream) -> ConnectionBuilder<'static> {
        ConnectionBuilder::unix_stream(stream)
    }
}

#[cfg(target_os = "windows")]
struct WindowsControlListener(WindowsUnixListener);

#[cfg(target_os = "windows")]
impl ControlListener for WindowsControlListener {
    type Stream = WindowsUnixStream;

    async fn bind(path: &Path) -> std::io::Result<Self> {
        WindowsUnixListener::bind(path).map(Self)
    }

    async fn probe(path: &Path) -> std::io::Result<()> {
        WindowsUnixStream::connect(path).map(|_| ())
    }

    async fn accept(&self) -> std::io::Result<WindowsUnixStream> {
        let listener = self.0.try_clone()?;
        tokio::task::spawn_blocking(move || listener.accept())
            .await
            .map_err(std::io::Error::other)?
            .map(|(stream, _)| stream)
    }

    fn authorize(&self, _stream: &WindowsUnixStream) -> std::io::Result<()> {
        // %TEMP% is per-user on Windows and uds_windows exposes no peer
        // credentials; the per-user socket directory is the boundary here.
        Ok(())
    }

    fn stream_builder(stream: WindowsUnixStream) -> ConnectionBuilder<'static> {
        ConnectionBuilder::unix_stream(stream)
    }
}

/// Bind the control listener, recovering from a stale socket file: if the
/// address is in use but nothing answers a probe, remove the file and rebind;
/// if a live daemon answers, refuse to start.
#[cfg(any(target_os = "macos", target_os = "windows"))]
async fn bind_control_listener<L: ControlListener>(socket_path: &Path) -> Result<L> {
    match L::bind(socket_path).await {
        Ok(listener) => Ok(listener),
        Err(bind_err) if bind_err.kind() == ErrorKind::AddrInUse => {
            match L::probe(socket_path).await {
                Ok(()) => bail!(
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
                    L::bind(socket_path).await.with_context(|| {
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

#[cfg(any(target_os = "macos", target_os = "windows"))]
async fn start_p2p_control_plane<L: ControlListener>(
    svc: ProxyService,
    state_rx: UnboundedReceiver<Box<str>>,
) -> Result<()> {
    let socket_path = p2p_control_socket_path();
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create control socket directory {}",
                parent.display()
            )
        })?;
        // Owner-only directory: the control interface has no authentication of
        // its own, so directory permissions keep other local users away from
        // the socket (and from squatting its path before we bind).
        #[cfg(target_os = "macos")]
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "failed to restrict control socket directory {}",
                    parent.display()
                )
            },
        )?;
    }
    let listener: L = bind_control_listener(&socket_path).await?;
    #[cfg(target_os = "macos")]
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).with_context(
        || {
            format!(
                "failed to restrict control socket {}",
                socket_path.display()
            )
        },
    )?;

    let peers = Arc::new(Mutex::new(HashMap::<u64, Connection>::new()));
    let next_peer_id = Arc::new(AtomicU64::new(1));
    spawn_p2p_state_signal_fanout(state_rx, Arc::clone(&peers));

    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!(error = %err, "control socket accept failed");
                    // Don't spin hot if accept keeps failing.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if let Err(err) = listener.authorize(&stream) {
                warn!(error = %err, "rejected unauthorized control connection");
                continue;
            }

            let builder = L::stream_builder(stream)
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
    mut state_rx: UnboundedReceiver<Box<str>>,
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
