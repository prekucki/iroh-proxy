use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use iroh::SecretKey;

pub fn default_key_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    Path::new(&home)
        .join(".config")
        .join("iroh-proxy")
        .join("secret_key")
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
