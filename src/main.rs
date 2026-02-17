use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod cli;
mod config;
mod control;
mod daemon;
mod forward;
mod keys;
mod remote_path;
mod serve;
mod tui;

use cli::{Cli, Commands};
use config::{ServeService, load_config};
use control::{
    add_forward as ctl_add_forward, add_serve as ctl_add_serve, del_serve as ctl_del_serve,
};
use daemon::run_server;
use forward::{ForwardBinding, forward_bindings, forward_stdio};
use keys::{load_or_create_forward_key, load_or_create_serve_key};
use remote_path::RemotePath;
use serve::serve_services;

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("iroh_proxy=info,portmapper.service=error,netlink_packet_route=error,info")
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Commands::Server => {
            let secret_key = load_or_create_serve_key(cli.key_file.as_deref())?;
            run_server(secret_key, Vec::new()).await
        }
        Commands::Status { connections } => {
            match control::status().await? {
                Some(status) => {
                    println!(
                        "running: true\nendpoint: {}\nconnections: {}\nserved: {}\nforwards: {}",
                        status.endpoint_id, status.connections, status.served, status.forwards
                    );
                    if connections {
                        let conns = control::list_connections().await?;
                        if conns.is_empty() {
                            println!("active-connections: none");
                        } else {
                            println!("active-connections:");
                            println!("{:<66}  {:<8}  dst", "src", "type");
                            for conn in conns {
                                println!("{:<66}  {:<8}  {}", conn.src, conn.kind, conn.dst);
                            }
                        }
                    }
                }
                None => {
                    println!("running: false");
                }
            }
            Ok(())
        }
        Commands::Tui => tui::run_tui().await,
        Commands::AddForward { listen, remote } => {
            ctl_add_forward(&listen, &remote)
                .await
                .with_context(|| "failed to add forward rule to running server")?;
            println!("added forward: {listen} -> {remote}");
            Ok(())
        }
        Commands::AddServe { name, target } => {
            ctl_add_serve(&name, &target)
                .await
                .with_context(|| "failed to add serve route to running server")?;
            println!("added serve: {name} -> {target}");
            Ok(())
        }
        Commands::DelServe { name } => {
            ctl_del_serve(&name)
                .await
                .with_context(|| "failed to remove serve route from running server")?;
            println!("deleted serve: {name}");
            Ok(())
        }
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
