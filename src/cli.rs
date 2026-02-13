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
}

#[derive(Debug, Subcommand)]
pub enum Commands {
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
