use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServeService {
    pub name: Box<str>,
    pub target: Box<str>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForwardService {
    pub listen: Box<str>,
    pub remote: Box<str>,
    #[serde(default = "default_close_on_request_timeout_secs")]
    pub close_on_request_timeout_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    pub serve: Option<ServeSection>,
    pub forward: Option<ForwardSection>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ServeSection {
    pub services: Vec<ServeService>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ForwardSection {
    pub services: Vec<ForwardService>,
}

fn default_close_on_request_timeout_secs() -> u64 {
    2
}

pub fn default_config_path() -> PathBuf {
    ProjectDirs::from("", "", "iroh-proxy")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| {
            PathBuf::from(".config")
                .join("iroh-proxy")
                .join("config.toml")
        })
}

pub fn load_config(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))
}

pub fn load_config_or_default(path: &Path) -> Result<Config> {
    if path.exists() {
        return load_config(path);
    }
    Ok(Config::default())
}

pub fn write_config(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }

    let raw = toml::to_string_pretty(config).context("failed to serialize config")?;
    std::fs::write(path, raw)
        .with_context(|| format!("failed to write config {}", path.display()))?;
    Ok(())
}

pub fn add_persistent_serve_rule(path: &Path, name: &str, target: &str) -> Result<()> {
    let mut config = load_config_or_default(path)?;

    let serve = config.serve.get_or_insert_with(ServeSection::default);
    if let Some(existing) = serve
        .services
        .iter()
        .find(|entry| entry.name.as_ref() == name)
    {
        if existing.target.as_ref() == target {
            return Ok(());
        }
        anyhow::bail!(
            "service '{}' already exists in {} with target '{}'",
            name,
            path.display(),
            existing.target
        );
    }

    serve.services.push(ServeService {
        name: name.into(),
        target: target.into(),
    });
    write_config(path, &config)
}

pub fn add_persistent_forward_rule(
    path: &Path,
    listen: &str,
    remote: &str,
    close_on_request_timeout_secs: u64,
) -> Result<()> {
    let mut config = load_config_or_default(path)?;

    let forward = config.forward.get_or_insert_with(ForwardSection::default);
    if let Some(existing) = forward
        .services
        .iter()
        .find(|entry| entry.listen.as_ref() == listen)
    {
        if existing.remote.as_ref() == remote
            && existing.close_on_request_timeout_secs == close_on_request_timeout_secs
        {
            return Ok(());
        }
        anyhow::bail!(
            "forward '{}' already exists in {} with remote '{}' and timeout {}s",
            listen,
            path.display(),
            existing.remote,
            existing.close_on_request_timeout_secs
        );
    }

    forward.services.push(ForwardService {
        listen: listen.into(),
        remote: remote.into(),
        close_on_request_timeout_secs,
    });
    write_config(path, &config)
}

pub fn remove_persistent_forward_rule(path: &Path, listen: &str, remote: &str) -> Result<bool> {
    remove_persistent_forward_rule_inner(path, |entry| {
        entry.listen.as_ref() == listen && entry.remote.as_ref() == remote
    })
}

pub fn remove_persistent_forward_rule_by_listen(path: &Path, listen: &str) -> Result<bool> {
    remove_persistent_forward_rule_inner(path, |entry| entry.listen.as_ref() == listen)
}

fn remove_persistent_forward_rule_inner(
    path: &Path,
    matches: impl Fn(&ForwardService) -> bool,
) -> Result<bool> {
    let mut config = load_config_or_default(path)?;
    let Some(forward) = config.forward.as_mut() else {
        return Ok(false);
    };

    let initial_len = forward.services.len();
    forward.services.retain(|entry| !matches(entry));
    if forward.services.len() == initial_len {
        return Ok(false);
    }

    if forward.services.is_empty() {
        config.forward = None;
    }
    write_config(path, &config)?;
    Ok(true)
}
