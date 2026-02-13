use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ServeService {
    pub name: Box<str>,
    pub target: Box<str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForwardService {
    pub listen: Box<str>,
    pub remote: Box<str>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub serve: Option<ServeSection>,
    pub forward: Option<ForwardSection>,
}

#[derive(Debug, Deserialize)]
pub struct ServeSection {
    pub services: Vec<ServeService>,
}

#[derive(Debug, Deserialize)]
pub struct ForwardSection {
    pub services: Vec<ForwardService>,
}

pub fn load_config(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))
}
