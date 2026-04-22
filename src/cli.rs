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

        /// Close iroh->tcp after this many seconds once tcp->iroh request upload EOF is reached
        #[arg(long, default_value_t = 2)]
        close_on_request_timeout_secs: u64,

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

    /// Remove a forward rule from the live proxy server
    DelForward {
        /// Also remove this forward rule from the config file
        #[arg(short = 'p', long)]
        persistent: bool,

        /// Local listen address in host:port form (same one used with add-forward)
        listen: String,
    },

    /// Forward to a remote iroh endpoint path.
    ///
    /// - One arg: stdio mode (useful for ssh ProxyCommand)
    /// - Two args: listen mode (<listen> <remote>)
    /// - With `--fdpass`: OpenSSH `ProxyUseFdpass yes` mode; pass a
    ///   connected socket back to ssh via SCM_RIGHTS on stdout and detach
    ///   the iroh relay into a background process.
    Forward {
        /// Close iroh->tcp after this many seconds once tcp->iroh request upload EOF is reached
        #[arg(long, default_value_t = 2)]
        close_on_request_timeout_secs: u64,

        /// OpenSSH ProxyUseFdpass mode: send a connected socket back to ssh
        /// via SCM_RIGHTS on stdout, then exit (relay runs detached).
        #[arg(long, conflicts_with = "fdpass_fd")]
        fdpass: bool,

        /// Internal: run as the detached fdpass relay child, reading/writing
        /// on the given inherited file descriptor.
        #[arg(long, hide = true, value_name = "FD")]
        fdpass_fd: Option<i32>,

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
