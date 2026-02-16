use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use clap::Parser;

mod cli;
mod config;
mod forward;
mod keys;
mod remote_path;
mod serve;

use cli::{Cli, Commands};
use config::{ServeService, load_config};
use forward::{ForwardBinding, forward_bindings, forward_stdio};
use keys::{load_or_create_forward_key, load_or_create_serve_key};
use remote_path::RemotePath;
use serve::serve_services;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { name, target } => {
            let secret_key = load_or_create_serve_key(cli.key_file.as_deref())?;
            let services = vec![ServeService {
                name: name.into(),
                target: target.into(),
            }];
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
                    listen: first.into(),
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
