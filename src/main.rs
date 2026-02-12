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
use keys::{default_key_path, load_or_create_secret_key};
use remote_path::RemotePath;
use serve::serve_services;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let key_path = cli.key_file.unwrap_or_else(default_key_path);
    let secret_key = load_or_create_secret_key(&key_path)?;

    match cli.command {
        Commands::Serve { name, target } => {
            let services = vec![ServeService { name, target }];
            serve_services(secret_key, services).await
        }
        Commands::ServeConfig { config } => {
            let cfg = load_config(&config)?;
            let serve = cfg
                .serve
                .ok_or_else(|| anyhow!("missing [serve] section in {}", config.display()))?;
            serve_services(secret_key, serve.services).await
        }
        Commands::Forward { first, second } => match second {
            Some(remote) => {
                let bindings = vec![ForwardBinding {
                    listen: first,
                    remote: RemotePath::from_str(&remote)?,
                }];
                forward_bindings(secret_key, bindings).await
            }
            None => {
                let remote = RemotePath::from_str(&first)?;
                forward_stdio(secret_key, remote).await
            }
        },
        Commands::ForwardConfig { config } => {
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
