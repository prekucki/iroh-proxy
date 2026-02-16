use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use iroh::{Endpoint, EndpointId, SecretKey};
use serde::Deserialize;
use tokio::io::{self, AsyncWriteExt};
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

    /// Serve multiple local TCP services from a TOML config file
    ServeConfig {
        /// Path to config.toml
        config: PathBuf,
    },

    /// Forward to a remote iroh endpoint path.
    ///
    /// - One arg: stdio mode (useful for ssh ProxyCommand)
    /// - Two args: listen mode (<listen> <remote>)
    Forward {
        /// Remote path in form: <node-id>/tcp/<name> OR local bind when providing two args
        first: String,

        /// Remote path in form: <node-id>/tcp/<name> (only required for listen mode)
        second: Option<String>,
    },

    /// Forward multiple local listeners from a TOML config file
    ForwardConfig {
        /// Path to config.toml
        config: PathBuf,
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

#[derive(Debug, Clone, Deserialize)]
struct ServeService {
    name: String,
    target: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ForwardService {
    listen: String,
    remote: String,
}

#[derive(Debug, Deserialize)]
struct Config {
    serve: Option<ServeSection>,
    forward: Option<ForwardSection>,
}

#[derive(Debug, Deserialize)]
struct ServeSection {
    services: Vec<ServeService>,
}

#[derive(Debug, Deserialize)]
struct ForwardSection {
    services: Vec<ForwardService>,
}

#[derive(Debug, Clone)]
struct Route {
    name: String,
    target: String,
}

#[derive(Debug, Clone)]
struct ForwardBinding {
    listen: String,
    remote: RemotePath,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { name, target } => {
            let secret_key = load_or_create_serve_key(cli.key_file.as_deref())?;
            let services = vec![ServeService { name, target }];
            serve_services(secret_key, services).await
        }
        Commands::ServeConfig { config } => {
            let secret_key = load_or_create_serve_key(cli.key_file.as_deref())?;
            let cfg = load_config(&config)?;
            let serve = cfg
                .serve
                .ok_or_else(|| anyhow!("missing [serve] section in {}", config.display()))?;
            serve_services(secret_key, serve.services).await
        }
        Commands::Forward { first, second } => match second {
            Some(remote) => {
                let secret_key = load_or_create_forward_key(cli.key_file.as_deref())?;
                let bindings = vec![ForwardBinding {
                    listen: first,
                    remote: RemotePath::from_str(&remote)?,
                }];
                forward_bindings(secret_key, bindings).await
            }
            None => {
                let secret_key = load_or_create_forward_key(cli.key_file.as_deref())?;
                let remote = RemotePath::from_str(&first)?;
                forward_stdio(secret_key, remote).await
            }
        },
        Commands::ForwardConfig { config } => {
            let secret_key = load_or_create_forward_key(cli.key_file.as_deref())?;
            let cfg = load_config(&config)?;
            let forward = cfg
                .forward
                .ok_or_else(|| anyhow!("missing [forward] section in {}", config.display()))?;

            let mut bindings = Vec::with_capacity(forward.services.len());
            for entry in forward.services {
                bindings.push(ForwardBinding {
                    listen: entry.listen,
                    remote: RemotePath::from_str(&entry.remote)
                        .with_context(|| format!("invalid remote path '{}'", entry.remote))?,
                });
            }

            forward_bindings(secret_key, bindings).await
        }
    }
}

async fn serve_services(secret_key: SecretKey, services: Vec<ServeService>) -> Result<()> {
    if services.is_empty() {
        bail!("at least one service is required");
    }

    let mut alpns = Vec::with_capacity(services.len());
    let mut routes = HashMap::<Vec<u8>, Route>::with_capacity(services.len());

    for service in services {
        if service.name.trim().is_empty() {
            bail!("service name cannot be empty");
        }

        let alpn = alpn_for_service(&service.name);
        if routes.contains_key(&alpn) {
            bail!("duplicate service name '{}'", service.name);
        }

        alpns.push(alpn.clone());
        routes.insert(
            alpn,
            Route {
                name: service.name,
                target: service.target,
            },
        );
    }

    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .alpns(alpns)
        .bind()
        .await?;

    endpoint.online().await;

    eprintln!("Serving endpoint id: {}", endpoint.id());
    eprintln!("Endpoint address: {:?}", endpoint.addr());
    for route in routes.values() {
        eprintln!("- {}/tcp/{} -> {}", endpoint.id(), route.name, route.target);
    }

    let routes = Arc::new(routes);
    loop {
        let incoming = endpoint.accept().await.context("endpoint closed")?;
        let routes = Arc::clone(&routes);

        tokio::spawn(async move {
            if let Err(err) = handle_incoming(incoming, routes).await {
                eprintln!("incoming connection error: {err:#}");
            }
        });
    }
}

async fn handle_incoming(
    incoming: iroh::endpoint::Incoming,
    routes: Arc<HashMap<Vec<u8>, Route>>,
) -> Result<()> {
    let conn = incoming.await?;
    let negotiated = conn.alpn();

    let route = routes
        .get(negotiated)
        .ok_or_else(|| {
            anyhow!(
                "unknown service ALPN '{}'",
                String::from_utf8_lossy(negotiated)
            )
        })?
        .clone();

    let (mut send, mut recv) = conn.accept_bi().await?;
    let local = TcpStream::connect(&route.target)
        .await
        .with_context(|| format!("failed to connect local target {}", route.target))?;

    let (mut local_read, mut local_write) = local.into_split();

    let to_local = io::copy(&mut recv, &mut local_write);
    let to_remote = io::copy(&mut local_read, &mut send);
    let _ = tokio::try_join!(to_local, to_remote)?;

    send.finish()?;
    Ok(())
}

async fn forward_stdio(secret_key: SecretKey, remote: RemotePath) -> Result<()> {
    let alpn = alpn_for_service(&remote.service);
    let endpoint = Endpoint::builder().secret_key(secret_key).bind().await?;
    endpoint.online().await;

    let conn = endpoint
        .connect(remote.endpoint_id, &alpn)
        .await
        .with_context(|| {
            format!(
                "failed connecting to remote endpoint {}",
                remote.endpoint_id
            )
        })?;
    let (mut send, mut recv) = conn.open_bi().await?;

    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    let to_remote = io::copy(&mut stdin, &mut send);
    let to_local = io::copy(&mut recv, &mut stdout);
    let _ = tokio::try_join!(to_remote, to_local)?;

    send.finish()?;
    stdout.flush().await?;
    Ok(())
}

async fn forward_bindings(secret_key: SecretKey, bindings: Vec<ForwardBinding>) -> Result<()> {
    if bindings.is_empty() {
        bail!("at least one forward binding is required");
    }

    let endpoint = Endpoint::builder().secret_key(secret_key).bind().await?;
    endpoint.online().await;

    let mut prepared = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let listener = TcpListener::bind(&binding.listen)
            .await
            .with_context(|| format!("failed to bind local listener {}", binding.listen))?;

        let alpn = alpn_for_service(&binding.remote.service);
        eprintln!(
            "Forwarding {} -> {}/tcp/{}",
            binding.listen, binding.remote.endpoint_id, binding.remote.service
        );

        prepared.push((listener, binding, alpn));
    }

    for (listener, binding, alpn) in prepared {
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            if let Err(err) = run_forward_listener(endpoint, listener, binding, alpn).await {
                eprintln!("forward listener error: {err:#}");
            }
        });
    }

    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}

async fn run_forward_listener(
    endpoint: Endpoint,
    listener: TcpListener,
    binding: ForwardBinding,
    alpn: Vec<u8>,
) -> Result<()> {
    loop {
        let (inbound, peer_addr) = listener.accept().await?;
        let endpoint = endpoint.clone();
        let remote = binding.remote.clone();
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
        .with_context(|| {
            format!(
                "failed connecting to remote endpoint {}",
                remote.endpoint_id
            )
        })?;

    let (mut send, mut recv) = conn.open_bi().await?;

    let (mut inbound_read, mut inbound_write) = inbound.into_split();

    let to_remote = io::copy(&mut inbound_read, &mut send);
    let to_local = io::copy(&mut recv, &mut inbound_write);
    let _ = tokio::try_join!(to_remote, to_local)?;

    send.finish()?;
    Ok(())
}

fn load_config(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))
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

fn load_or_create_serve_key(key_file: Option<&Path>) -> Result<SecretKey> {
    if let Some(path) = key_file {
        return load_or_create_secret_key(path);
    }
    let default = default_key_path();
    load_or_create_secret_key(&default)
}

fn load_or_create_forward_key(key_file: Option<&Path>) -> Result<SecretKey> {
    if let Some(path) = key_file {
        return load_or_create_secret_key(path);
    }
    let mut rng = rand::rng();
    Ok(SecretKey::generate(&mut rng))
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
