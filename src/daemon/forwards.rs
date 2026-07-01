//! Client-side forward bindings owned by the daemon: local TCP listeners whose
//! accepted connections are forwarded to a remote service.

use anyhow::{Context, Result, bail};
use iroh::Endpoint;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{JoinHandle, JoinSet};
use tracing::{error, warn};

use crate::forward::ForwardBinding;
use crate::proxy::forward_tcp_conn;
use crate::remote_path::RemotePath;

use super::Forwards;
use super::connections::{ActiveConnection, ConnectionRegistry};

#[derive(Debug)]
pub(super) struct ForwardRuntime {
    pub(super) remote: RemotePath,
    pub(super) persisted: bool,
    task: JoinHandle<()>,
}

impl Drop for ForwardRuntime {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn add_forward_binding(
    endpoint: Endpoint,
    forwards: Forwards,
    connections: ConnectionRegistry,
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
    let close_on_request_timeout = binding.close_on_request_timeout;

    let task = tokio::spawn(async move {
        // Track per-connection tasks so that aborting this listener task (on
        // del-forward / ForwardRuntime::Drop) also tears down in-flight
        // connections: dropping the JoinSet aborts every task it holds.
        let mut conns: JoinSet<()> = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (inbound, peer_addr) = match accepted {
                        Ok(pair) => pair,
                        Err(err) => {
                            error!(error = %err, listen = %listen, "forward listener accept failed");
                            return;
                        }
                    };

                    let endpoint = endpoint.clone();
                    let remote = remote.clone();
                    let connections = connections.clone();
                    conns.spawn(async move {
                        let register = |src: &str| {
                            connections.register(ActiveConnection {
                                src: src.into(),
                                kind: "forward".into(),
                                dst: format!("{}/tcp/{}", remote.endpoint_id, remote.service)
                                    .into(),
                            })
                        };
                        if let Err(err) = forward_tcp_conn(
                            &endpoint,
                            inbound,
                            &remote,
                            close_on_request_timeout,
                            register,
                        )
                        .await
                        {
                            warn!(
                                target: "iroh_proxy::forward",
                                peer = %peer_addr,
                                error = %format!("{err:#}"),
                                "forwarding connection failed"
                            );
                        }
                    });
                }
                // Reap finished connection tasks to bound memory.
                Some(_joined) = conns.join_next(), if !conns.is_empty() => {}
            }
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
