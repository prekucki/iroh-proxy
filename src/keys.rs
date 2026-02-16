use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use iroh::SecretKey;

pub fn default_key_path() -> PathBuf {
    ProjectDirs::from("", "", "iroh-proxy")
        .map(|dirs| dirs.config_dir().join("secret_key"))
        .unwrap_or_else(|| PathBuf::from(".config/iroh-proxy/secret_key"))
}

pub fn load_or_create_secret_key(path: &Path) -> Result<SecretKey> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create key dir {}", parent.display()))?;
    }

    if path.exists() {
        let raw = std::fs::read(path)
            .with_context(|| format!("failed to read key file {}", path.display()))?;
        return SecretKey::try_from(raw.as_slice())
            .with_context(|| format!("invalid key in {}", path.display()));
    }

    let mut rng = rand::rng();
    let sk = SecretKey::generate(&mut rng);
    std::fs::write(path, sk.to_bytes())
        .with_context(|| format!("failed to write key file {}", path.display()))?;
    Ok(sk)
}

pub fn load_or_create_serve_key(key_file: Option<&Path>) -> Result<SecretKey> {
    if let Some(path) = key_file {
        return load_or_create_secret_key(path);
    }
    let default = default_key_path();
    load_or_create_secret_key(&default)
}

pub fn load_or_create_forward_key(key_file: Option<&Path>) -> Result<SecretKey> {
    if let Some(path) = key_file {
        return load_or_create_secret_key(path);
    }
    let mut rng = rand::rng();
    Ok(SecretKey::generate(&mut rng))
}
