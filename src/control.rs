use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};
use zbus::{Connection, Proxy};

pub const BUS_NAME: &str = "dev.iroh.Proxy";
pub const OBJECT_PATH: &str = "/dev/iroh/Proxy";
pub const INTERFACE: &str = "dev.iroh.Proxy";

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

pub async fn status() -> Result<Option<Status>> {
    let conn = match Connection::session().await {
        Ok(conn) => conn,
        Err(_) => return Ok(None),
    };
    let proxy = match Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE).await {
        Ok(proxy) => proxy,
        Err(_) => return Ok(None),
    };

    let call: Result<(String, u64, u64, u64), zbus::Error> = proxy.call("Status", &()).await;
    match call {
        Ok((endpoint_id, connections, served, forwards)) => Ok(Some(Status {
            endpoint_id: endpoint_id.into(),
            connections,
            served,
            forwards,
        })),
        Err(_) => Ok(None),
    }
}

pub async fn add_forward(listen: &str, remote: &str, persisted: bool) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("failed to connect to DBus session bus")?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("failed to connect to iroh-proxy control interface")?;
    let _: () = proxy
        .call("AddForward", &(listen, remote, persisted))
        .await
        .map_err(|err| anyhow!("AddForward failed: {err}"))?;
    Ok(())
}

pub async fn list_connections() -> Result<Vec<ActiveConnection>> {
    let conn = Connection::session()
        .await
        .context("failed to connect to DBus session bus")?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("failed to connect to iroh-proxy control interface")?;

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

pub async fn list_serves() -> Result<Vec<ServeRoute>> {
    let conn = Connection::session()
        .await
        .context("failed to connect to DBus session bus")?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("failed to connect to iroh-proxy control interface")?;

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

pub async fn list_forwards() -> Result<Vec<ForwardRoute>> {
    let conn = Connection::session()
        .await
        .context("failed to connect to DBus session bus")?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("failed to connect to iroh-proxy control interface")?;

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

pub async fn del_forward(listen: &str) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("failed to connect to DBus session bus")?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("failed to connect to iroh-proxy control interface")?;
    let _: () = proxy
        .call("DelForward", &(listen,))
        .await
        .map_err(|err| anyhow!("DelForward failed: {err}"))?;
    Ok(())
}

pub async fn add_serve(name: &str, target: &str) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("failed to connect to DBus session bus")?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("failed to connect to iroh-proxy control interface")?;
    let _: () = proxy
        .call("AddServe", &(name, target))
        .await
        .map_err(|err| anyhow!("AddServe failed: {err}"))?;
    Ok(())
}

pub async fn del_serve(name: &str) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("failed to connect to DBus session bus")?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("failed to connect to iroh-proxy control interface")?;
    let _: () = proxy
        .call("DelServe", &(name,))
        .await
        .map_err(|err| anyhow!("DelServe failed: {err}"))?;
    Ok(())
}

pub fn watch_state_changes() -> mpsc::UnboundedReceiver<Box<str>> {
    let (tx, rx) = mpsc::unbounded_channel::<Box<str>>();
    tokio::spawn(async move {
        loop {
            let conn = match Connection::session().await {
                Ok(conn) => conn,
                Err(_) => {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let proxy = match Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE).await {
                Ok(proxy) => proxy,
                Err(_) => {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let mut stream = match proxy.receive_signal("StateChanged").await {
                Ok(stream) => stream,
                Err(_) => {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            while let Some(msg) = stream.next().await {
                if let Ok((reason,)) = msg.body().deserialize::<(String,)>() {
                    if tx.send(reason.into()).is_err() {
                        return;
                    }
                } else if tx.send("state-changed".into()).is_err() {
                    return;
                }
            }

            sleep(Duration::from_millis(300)).await;
        }
    });
    rx
}
