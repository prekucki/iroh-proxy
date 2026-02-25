use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use iroh::SecretKey;

pub struct ServeKeyLock {
    file: File,
}

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

fn load_or_create_secret_key_from_file(file: &mut File, path: &Path) -> Result<SecretKey> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to seek key file {}", path.display()))?;

    let mut raw = Vec::new();
    file.read_to_end(&mut raw)
        .with_context(|| format!("failed to read key file {}", path.display()))?;

    if raw.is_empty() {
        let mut rng = rand::rng();
        let sk = SecretKey::generate(&mut rng);
        file.set_len(0)
            .with_context(|| format!("failed to truncate key file {}", path.display()))?;
        file.seek(SeekFrom::Start(0))
            .with_context(|| format!("failed to seek key file {}", path.display()))?;
        file.write_all(&sk.to_bytes())
            .with_context(|| format!("failed to write key file {}", path.display()))?;
        file.sync_data()
            .with_context(|| format!("failed to sync key file {}", path.display()))?;
        return Ok(sk);
    }

    SecretKey::try_from(raw.as_slice())
        .with_context(|| format!("invalid key in {}", path.display()))
}

fn serve_key_path(key_file: Option<&Path>) -> PathBuf {
    key_file
        .map(ToOwned::to_owned)
        .unwrap_or_else(default_key_path)
}

pub fn lock_serve_key_file(key_file: Option<&Path>) -> Result<ServeKeyLock> {
    let path = serve_key_path(key_file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create key dir {}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open key file {}", path.display()))?;

    file.try_lock().map_err(|err| {
        anyhow!(
            "failed to lock key file {} (another iroh-proxy server may already be running): {}",
            path.display(),
            err
        )
    })?;

    Ok(ServeKeyLock { file })
}

pub fn load_or_create_serve_key_and_lock(
    key_file: Option<&Path>,
) -> Result<(ServeKeyLock, SecretKey)> {
    let path = serve_key_path(key_file);
    let mut lock = lock_serve_key_file(key_file)?;
    let key = load_or_create_secret_key_from_file(&mut lock.file, &path)?;
    Ok((lock, key))
}

pub fn load_or_create_forward_key(key_file: Option<&Path>) -> Result<SecretKey> {
    if let Some(path) = key_file {
        return load_or_create_secret_key(path);
    }
    let mut rng = rand::rng();
    Ok(SecretKey::generate(&mut rng))
}
