use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "iroh-proxy")]
#[command(about = "TCP forwarding over iroh")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Path to persistent iroh secret key (defaults to ~/.config/iroh-proxy/secret_key)
    #[arg(long)]
    pub key_file: Option<PathBuf>,

    /// Path to proxy config file (defaults to ~/.config/iroh-proxy/config.toml)
    #[arg(long)]
    pub config_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run a long-lived proxy server with platform-native control interface
    Server {
        /// Install a user systemd unit at ~/.config/systemd/user/iroh-proxy.service
        #[arg(long)]
        install: bool,
    },

    /// Show if the live proxy server is running and current connection count
    Status {
        /// Also print active connections (src, type, dst)
        #[arg(long)]
        connections: bool,
    },

    /// Open a simple terminal UI for live server inspection
    Tui,

    /// Add a forward rule to the live proxy server
    AddForward {
        /// Persist this forward rule to config file
        #[arg(short = 'p', long)]
        persistent: bool,

        /// Local listen address in host:port form
        listen: String,
        /// Remote path in form: <node-id>/tcp/<name>
        remote: String,
    },

    /// Add a served TCP service to the live proxy server
    AddServe {
        /// Persist this serve rule to config file
        #[arg(short = 'p', long)]
        persistent: bool,

        /// Service name used in remote path: <node-id>/tcp/<name>
        name: String,
        /// Local target in host:port form
        target: String,
    },

    /// Remove a served TCP service from the live proxy server
    DelServe {
        /// Service name used in remote path: <node-id>/tcp/<name>
        name: String,
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
