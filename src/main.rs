use std::path::Path;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;
use std::{cmp, io::IsTerminal};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod cli;
mod config;
mod control;
mod daemon;
mod forward;
mod keys;
mod remote_path;
#[cfg(target_os = "linux")]
mod systemd;
mod tui;

use cli::{Cli, Commands};
use config::{
    add_persistent_forward_rule, add_persistent_serve_rule, default_config_path, load_config,
    load_config_or_default,
};
use control::{
    add_forward as ctl_add_forward, add_serve as ctl_add_serve, del_serve as ctl_del_serve,
};
use daemon::run_server;
use forward::{ForwardBinding, forward_bindings, forward_stdio};
use keys::{load_or_create_forward_key, load_or_create_serve_key_and_lock};
use remote_path::RemotePath;

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "iroh_proxy=info,mainline::rpc::socket=error,portmapper.service=error,netlink_packet_route=error,info",
        )
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

async fn ensure_server_running(key_file: Option<&Path>, config_file: &Path) -> Result<()> {
    let caps = control::capabilities();
    if !caps.live_control {
        bail!(
            "live control commands are not available on {} yet (planned transport: {})",
            std::env::consts::OS,
            caps.transport_label
        );
    }

    if control::status().await?.is_some() {
        return Ok(());
    }

    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    let mut command = std::process::Command::new(exe);
    if let Some(path) = key_file {
        command.arg("--key-file").arg(path);
    }
    command.arg("--config-file").arg(config_file);
    command
        .arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .context("failed to start background server process")?;

    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(200));
        if control::status().await?.is_some() {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .context("failed while waiting for server startup")?
        {
            return Err(anyhow!("server exited while starting ({status})"));
        }
    }

    Err(anyhow!(
        "timed out waiting for server to become available on {}",
        caps.transport_label
    ))
}

fn style(text: &str, code: &str, use_ansi: bool) -> String {
    if use_ansi {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn print_status_running(status: &control::Status, use_ansi: bool) {
    println!("{}", style("iroh-proxy status", "1", use_ansi));
    println!("{:<12} {}", "server", style("running", "32", use_ansi));
    println!("{:<12} {}", "endpoint", status.endpoint_id);
    println!();
    println!("{}", style("counters", "1", use_ansi));
    println!("{:<12} {}", "connections", status.connections);
    println!("{:<12} {}", "served", status.served);
    println!("{:<12} {}", "forwards", status.forwards);
}

fn print_active_connections(mut conns: Vec<control::ActiveConnection>, use_ansi: bool) {
    println!();
    println!(
        "{}",
        style(
            &format!("active connections ({})", conns.len()),
            "1",
            use_ansi
        )
    );
    if conns.is_empty() {
        println!("none");
        return;
    }

    conns.sort_by_key(|conn| conn.id);
    let id_width = cmp::max(
        2,
        conns
            .iter()
            .map(|conn| conn.id.to_string().len())
            .max()
            .unwrap_or(2),
    );
    let kind_width = cmp::max(
        "type".len(),
        conns.iter().map(|conn| conn.kind.len()).max().unwrap_or(4),
    );
    let src_width = cmp::max(
        "source".len(),
        conns.iter().map(|conn| conn.src.len()).max().unwrap_or(6),
    );

    println!(
        "{:>id_width$}  {:<kind_width$}  {:<src_width$}  destination",
        "id", "type", "source",
    );
    for conn in conns {
        println!(
            "{:>id_width$}  {:<kind_width$}  {:<src_width$}  {}",
            conn.id, conn.kind, conn.src, conn.dst
        );
    }
}

fn print_status_stopped(use_ansi: bool) {
    println!("{}", style("iroh-proxy status", "1", use_ansi));
    println!("{:<12} {}", "server", style("stopped", "31", use_ansi));
    println!("{:<12} run `iroh-proxy server`", "hint");
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let config_path = cli.config_file.clone().unwrap_or_else(default_config_path);

    match cli.command {
        Commands::Server { install } => {
            if install {
                #[cfg(not(target_os = "linux"))]
                {
                    bail!("server --install is only available on Linux (systemd)");
                }
                #[cfg(target_os = "linux")]
                {
                    let exe = std::env::current_exe()
                        .context("failed to resolve current executable path for service install")?;
                    let service_path =
                        systemd::install_user_service(&exe, cli.key_file.as_deref(), &config_path)?;
                    println!("installed: {}", service_path.display());
                    println!("next:");
                    println!("  systemctl --user daemon-reload");
                    println!("  systemctl --user enable --now iroh-proxy.service");
                    return Ok(());
                }
            }
            let (_key_lock, secret_key) =
                load_or_create_serve_key_and_lock(cli.key_file.as_deref())?;
            let config = load_config_or_default(&config_path)?;
            let initial_services = config
                .serve
                .map(|section| section.services)
                .unwrap_or_default();
            let initial_forwards = config
                .forward
                .map(|section| section.services)
                .unwrap_or_default();
            run_server(secret_key, initial_services, initial_forwards).await
        }
        Commands::Status { connections } => {
            let caps = control::capabilities();
            if !caps.live_control {
                println!(
                    "status is not available on {} (planned transport: {})",
                    std::env::consts::OS,
                    caps.transport_label
                );
                return Ok(());
            }
            let use_ansi = std::io::stdout().is_terminal();
            match control::status().await? {
                Some(status) => {
                    print_status_running(&status, use_ansi);
                    if connections {
                        let conns = control::list_connections().await?;
                        print_active_connections(conns, use_ansi);
                    }
                }
                None => {
                    print_status_stopped(use_ansi);
                }
            }
            Ok(())
        }
        Commands::Tui => {
            let caps = control::capabilities();
            if !caps.live_control || !caps.state_stream {
                bail!(
                    "tui requires live control + state stream support (platform: {}, planned transport: {})",
                    std::env::consts::OS,
                    caps.transport_label
                );
            }
            tui::run_tui(&config_path, cli.key_file.as_deref()).await
        }
        Commands::AddForward {
            persistent,
            close_on_request_timeout_secs,
            listen,
            remote,
        } => {
            ensure_server_running(cli.key_file.as_deref(), &config_path).await?;
            ctl_add_forward(&listen, &remote, persistent, close_on_request_timeout_secs)
                .await
                .with_context(|| "failed to add forward rule to running server")?;
            if persistent {
                add_persistent_forward_rule(
                    &config_path,
                    &listen,
                    &remote,
                    close_on_request_timeout_secs,
                )
                .with_context(|| {
                    format!(
                        "failed to persist forward rule to config {}",
                        config_path.display()
                    )
                })?;
                println!(
                    "persisted forward rule in {}: {listen} -> {remote}",
                    config_path.display()
                );
            }
            println!("added forward: {listen} -> {remote}");
            Ok(())
        }
        Commands::AddServe {
            persistent,
            name,
            target,
        } => {
            ensure_server_running(cli.key_file.as_deref(), &config_path).await?;
            ctl_add_serve(&name, &target)
                .await
                .with_context(|| "failed to add serve route to running server")?;
            if persistent {
                add_persistent_serve_rule(&config_path, &name, &target).with_context(|| {
                    format!(
                        "failed to persist serve rule to config {}",
                        config_path.display()
                    )
                })?;
                println!(
                    "persisted serve rule in {}: {name} -> {target}",
                    config_path.display()
                );
            }
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
        Commands::Forward {
            close_on_request_timeout_secs,
            first,
            second,
        } => match second {
            Some(remote) => {
                let secret_key = load_or_create_forward_key(cli.key_file.as_deref())?;
                let bindings = vec![ForwardBinding {
                    listen: first.into(),
                    remote: RemotePath::from_str(&remote)?,
                    close_on_request_timeout: Duration::from_secs(close_on_request_timeout_secs),
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
                    close_on_request_timeout: Duration::from_secs(
                        entry.close_on_request_timeout_secs,
                    ),
                });
            }

            forward_bindings(secret_key, bindings).await
        }
    }
}
