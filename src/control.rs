use anyhow::{Context, Result, anyhow};
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
    pub src: Box<str>,
    pub kind: Box<str>,
    pub dst: Box<str>,
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

pub async fn add_forward(listen: &str, remote: &str) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("failed to connect to DBus session bus")?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("failed to connect to iroh-proxy control interface")?;
    let _: () = proxy
        .call("AddForward", &(listen, remote))
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

    let rows: Vec<(String, String, String)> = proxy
        .call("ListConnections", &())
        .await
        .map_err(|err| anyhow!("ListConnections failed: {err}"))?;
    Ok(rows
        .into_iter()
        .map(|(src, kind, dst)| ActiveConnection {
            src: src.into(),
            kind: kind.into(),
            dst: dst.into(),
        })
        .collect())
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
