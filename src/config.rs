use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::remote_path::{RemotePath, validate_service_name};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    write_config_atomic(path, raw.as_bytes())
}

fn write_config_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = temp_config_path(path);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| format!("failed to create temp config {}", temp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write temp config {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync temp config {}", temp_path.display()))?;
        replace_file(&temp_path, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }

    result
}

fn temp_config_path(path: &Path) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        counter
    ))
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).with_context(|| format!("failed to replace config {}", to.display()))
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        std::fs::remove_file(to)
            .with_context(|| format!("failed to remove existing config {}", to.display()))?;
    }
    std::fs::rename(from, to).with_context(|| format!("failed to replace config {}", to.display()))
}

pub fn add_persistent_serve_rule(path: &Path, name: &str, target: &str) -> Result<()> {
    validate_service_name(name).with_context(|| format!("invalid service name '{name}'"))?;
    if target.trim().is_empty() {
        bail!("target cannot be empty");
    }

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
    if listen.trim().is_empty() {
        bail!("listen address cannot be empty");
    }
    remote
        .parse::<RemotePath>()
        .with_context(|| format!("invalid remote path '{remote}'"))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const ENDPOINT_ID: &str = "74f3645e8016bb34970c516acde5240e85ed4387dbe3aeb9189f50db5525bd76";

    fn temp_config(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "iroh-proxy-config-test-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("config.toml")
    }

    #[test]
    fn persistent_serve_rejects_invalid_service_name() {
        let path = temp_config("invalid-service");
        let err = add_persistent_serve_rule(&path, "bad/name", "127.0.0.1:1")
            .expect_err("invalid service name should fail");

        assert!(err.to_string().contains("invalid service name"));
        assert!(!path.exists());
    }

    #[test]
    fn persistent_forward_rejects_invalid_remote() {
        let path = temp_config("invalid-forward");
        let err = add_persistent_forward_rule(&path, "127.0.0.1:1", "not-a-remote", 2)
            .expect_err("invalid remote path should fail");

        assert!(err.to_string().contains("invalid remote path"));
        assert!(!path.exists());
    }

    #[test]
    fn write_config_round_trips_atomically_written_toml() {
        let path = temp_config("round-trip");
        add_persistent_serve_rule(&path, "ollama", "127.0.0.1:11434").expect("persist serve");
        add_persistent_forward_rule(
            &path,
            "127.0.0.1:11435",
            &format!("{ENDPOINT_ID}/tcp/ollama"),
            2,
        )
        .expect("persist forward");

        let config = load_config(&path).expect("load written config");
        assert_eq!(
            config.serve.expect("serve section").services[0]
                .name
                .as_ref(),
            "ollama"
        );
        assert_eq!(
            config.forward.expect("forward section").services[0]
                .listen
                .as_ref(),
            "127.0.0.1:11435"
        );
    }
}
