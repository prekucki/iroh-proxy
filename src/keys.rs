use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use iroh::SecretKey;
use rand::SeedableRng;
use rand::rngs::StdRng;

pub struct ServeKeyLock {
    file: File,
}

pub fn default_key_path() -> PathBuf {
    ProjectDirs::from("", "", "iroh-proxy")
        .map(|dirs| dirs.config_dir().join("secret_key"))
        .unwrap_or_else(|| {
            PathBuf::from(".config")
                .join("iroh-proxy")
                .join("secret_key")
        })
}

pub fn load_or_create_secret_key(path: &Path) -> Result<SecretKey> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create key dir {}", parent.display()))?;
    }

    match std::fs::read(path) {
        Ok(raw) => {
            restrict_secret_key_permissions(path)?;
            return SecretKey::try_from(raw.as_slice())
                .with_context(|| format!("invalid key in {}", path.display()));
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read key file {}", path.display()));
        }
    }

    let sk = SecretKey::generate(&mut StdRng::from_os_rng());
    let mut file = match open_secret_key_for_create(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            return load_or_create_secret_key(path);
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to create key file {}", path.display()));
        }
    };
    file.write_all(&sk.to_bytes())
        .with_context(|| format!("failed to write key file {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("failed to sync key file {}", path.display()))?;
    restrict_secret_key_permissions(path)?;
    Ok(sk)
}

fn open_secret_key_for_create(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

#[cfg(unix)]
fn restrict_secret_key_permissions(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect key file {}", path.display()))?;
    let mut permissions = metadata.permissions();
    let mode = permissions.mode() & 0o777;
    if mode != 0o600 {
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to restrict key file {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_secret_key_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn load_or_create_secret_key_from_file(file: &mut File, path: &Path) -> Result<SecretKey> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to seek key file {}", path.display()))?;

    let mut raw = Vec::new();
    file.read_to_end(&mut raw)
        .with_context(|| format!("failed to read key file {}", path.display()))?;

    if raw.is_empty() {
        let sk = SecretKey::generate(&mut StdRng::from_os_rng());
        restrict_secret_key_permissions(path)?;
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

    restrict_secret_key_permissions(path)?;
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

    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(&path)
        .with_context(|| format!("failed to open key file {}", path.display()))?;

    file.try_lock().map_err(|err| {
        anyhow!(
            "failed to lock key file {} (another iroh-proxy server may already be running): {}",
            path.display(),
            err
        )
    })?;
    restrict_secret_key_permissions(&path)?;

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
    Ok(SecretKey::generate(&mut StdRng::from_os_rng()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    fn temp_key_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "iroh-proxy-key-test-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("secret_key")
    }

    #[cfg(unix)]
    #[test]
    fn load_or_create_secret_key_creates_private_file() {
        let path = temp_key_path("create-private");

        load_or_create_secret_key(&path).expect("create key");
        let mode = std::fs::metadata(&path)
            .expect("key metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn load_or_create_secret_key_tightens_existing_file() {
        let path = temp_key_path("tighten-existing");

        load_or_create_secret_key(&path).expect("create key");
        let mut permissions = std::fs::metadata(&path)
            .expect("key metadata")
            .permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&path, permissions).expect("loosen permissions");

        load_or_create_secret_key(&path).expect("load key");
        let mode = std::fs::metadata(&path)
            .expect("key metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600);
    }
}
