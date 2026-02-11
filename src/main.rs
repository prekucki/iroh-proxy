use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use iroh::{Endpoint, EndpointId, SecretKey};
use tokio::io;
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Parser)]
#[command(name = "iroh-proxy")]
#[command(about = "TCP forwarding over iroh")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to persistent iroh secret key (defaults to ~/.config/iroh-proxy/secret_key)
    #[arg(long)]
    key_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Serve a local TCP service over iroh as a named endpoint
    Serve {
        /// Service name used in remote path: <node-id>/tcp/<name>
        #[arg(long)]
        name: String,

        /// Local target in host:port form, e.g. localhost:11434
        target: String,
    },

    /// Forward a local TCP listener to a remote iroh endpoint path
    Forward {
        /// Local bind address, e.g. 127.0.0.1:11435
        listen: String,

        /// Remote path in form: <node-id>/tcp/<name>
        remote: String,
    },
}

#[derive(Debug, Clone)]
struct RemotePath {
    endpoint_id: EndpointId,
    service: String,
}

impl FromStr for RemotePath {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut parts = value.split('/');
        let endpoint_raw = parts
            .next()
            .ok_or_else(|| anyhow!("missing endpoint id in remote path"))?;
        let protocol = parts
            .next()
            .ok_or_else(|| anyhow!("missing protocol segment in remote path"))?;
        let service = parts
            .next()
            .ok_or_else(|| anyhow!("missing service segment in remote path"))?;

        if parts.next().is_some() {
            bail!("remote path must be exactly <node-id>/tcp/<name>");
        }
        if protocol != "tcp" {
            bail!("unsupported protocol '{protocol}', expected 'tcp'");
        }

        let endpoint_id = EndpointId::from_str(endpoint_raw)
            .with_context(|| format!("invalid endpoint id in remote path: {endpoint_raw}"))?;

        Ok(Self {
            endpoint_id,
            service: service.to_string(),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let key_path = cli.key_file.unwrap_or_else(default_key_path);
    let secret_key = load_or_create_secret_key(&key_path)?;

    match cli.command {
        Commands::Serve { name, target } => serve(secret_key, name, target).await,
        Commands::Forward { listen, remote } => forward(secret_key, listen, remote).await,
    }
}

async fn serve(secret_key: SecretKey, name: String, target: String) -> Result<()> {
    let alpn = alpn_for_service(&name);
    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .alpns(vec![alpn.clone()])
        .bind()
        .await?;

    endpoint.online().await;

    eprintln!("Serving {target} as {}/tcp/{name}", endpoint.id());
    eprintln!("Endpoint address: {:?}", endpoint.addr());

    loop {
        let incoming = endpoint.accept().await.context("endpoint closed")?;
        let target = target.clone();
        let alpn = alpn.clone();

        tokio::spawn(async move {
            if let Err(err) = handle_incoming(incoming, target, alpn).await {
                eprintln!("incoming connection error: {err:#}");
            }
        });
    }
}

async fn handle_incoming(
    incoming: iroh::endpoint::Incoming,
    target: String,
    expected_alpn: Vec<u8>,
) -> Result<()> {
    let conn = incoming.await?;

    let negotiated = conn.alpn();
    if negotiated != expected_alpn {
        bail!(
            "unexpected ALPN '{}', expected '{}'",
            String::from_utf8_lossy(negotiated),
            String::from_utf8_lossy(&expected_alpn)
        );
    }

    let (mut send, mut recv) = conn.accept_bi().await?;
    let local = TcpStream::connect(&target)
        .await
        .with_context(|| format!("failed to connect local target {target}"))?;

    let (mut local_read, mut local_write) = local.into_split();

    let to_local = io::copy(&mut recv, &mut local_write);
    let to_remote = io::copy(&mut local_read, &mut send);
    let _ = tokio::try_join!(to_local, to_remote)?;

    send.finish()?;
    Ok(())
}

async fn forward(secret_key: SecretKey, listen: String, remote: String) -> Result<()> {
    let remote = RemotePath::from_str(&remote)?;
    let alpn = alpn_for_service(&remote.service);

    let endpoint = Endpoint::builder().secret_key(secret_key).bind().await?;
    endpoint.online().await;

    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("failed to bind local listener {listen}"))?;

    eprintln!(
        "Forwarding {listen} -> {}/tcp/{}",
        remote.endpoint_id, remote.service
    );

    loop {
        let (inbound, peer_addr) = listener.accept().await?;
        let endpoint = endpoint.clone();
        let remote = remote.clone();
        let alpn = alpn.clone();

        tokio::spawn(async move {
            if let Err(err) = handle_forward_conn(endpoint, inbound, remote, alpn).await {
                eprintln!("forwarding {peer_addr} failed: {err:#}");
            }
        });
    }
}

async fn handle_forward_conn(
    endpoint: Endpoint,
    inbound: TcpStream,
    remote: RemotePath,
    alpn: Vec<u8>,
) -> Result<()> {
    let conn = endpoint
        .connect(remote.endpoint_id, &alpn)
        .await
        .with_context(|| format!("failed connecting to remote endpoint {}", remote.endpoint_id))?;

    let (mut send, mut recv) = conn.open_bi().await?;

    let (mut inbound_read, mut inbound_write) = inbound.into_split();

    let to_remote = io::copy(&mut inbound_read, &mut send);
    let to_local = io::copy(&mut recv, &mut inbound_write);
    let _ = tokio::try_join!(to_remote, to_local)?;

    send.finish()?;
    Ok(())
}

fn alpn_for_service(name: &str) -> Vec<u8> {
    format!("iroh-proxy/tcp/{name}").into_bytes()
}

fn default_key_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    Path::new(&home)
        .join(".config")
        .join("iroh-proxy")
        .join("secret_key")
}

fn load_or_create_secret_key(path: &Path) -> Result<SecretKey> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create key dir {}", parent.display()))?;
    }

    if path.exists() {
        let raw = std::fs::read(path)
            .with_context(|| format!("failed to read key file {}", path.display()))?;
        return SecretKey::try_from(raw.as_slice())
            .with_context(|| format!("invalid key in {}", path.display()));
    }

    let mut rng = rand::rng();
    let sk = SecretKey::generate(&mut rng);
    std::fs::write(path, sk.to_bytes())
        .with_context(|| format!("failed to write key file {}", path.display()))?;
    Ok(sk)
}
