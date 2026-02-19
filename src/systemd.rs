use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::BaseDirs;

const USER_SERVICE_NAME: &str = "iroh-proxy.service";

pub fn install_user_service(
    exe_path: &Path,
    key_file: Option<&Path>,
    config_file: &Path,
) -> Result<PathBuf> {
    let service_path = user_service_path()?;
    if let Some(parent) = service_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create systemd user dir {}", parent.display()))?;
    }

    let key_file = key_file.map(absolutize_path).transpose()?;
    let config_file = absolutize_path(config_file)?;
    let unit = render_unit(exe_path, key_file.as_deref(), &config_file);
    std::fs::write(&service_path, unit).with_context(|| {
        format!(
            "failed to write systemd user service {}",
            service_path.display()
        )
    })?;
    Ok(service_path)
}

fn user_service_path() -> Result<PathBuf> {
    let base =
        BaseDirs::new().context("failed to locate home directory for systemd user unit path")?;
    Ok(base
        .config_dir()
        .join("systemd/user")
        .join(USER_SERVICE_NAME))
}

fn render_unit(exe_path: &Path, key_file: Option<&Path>, config_file: &Path) -> String {
    let exec_start = render_exec_start(exe_path, key_file, config_file);
    format!(
        "[Unit]\n\
Description=iroh-proxy server\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={exec_start}\n\
Restart=on-failure\n\
RestartSec=2\n\
\n\
[Install]\n\
WantedBy=default.target\n"
    )
}

fn render_exec_start(exe_path: &Path, key_file: Option<&Path>, config_file: &Path) -> String {
    let mut args = vec![quote_systemd_arg(&exe_path.to_string_lossy())];
    if let Some(key_file) = key_file {
        args.push(quote_systemd_arg("--key-file"));
        args.push(quote_systemd_arg(&key_file.to_string_lossy()));
    }
    args.push(quote_systemd_arg("--config-file"));
    args.push(quote_systemd_arg(&config_file.to_string_lossy()));
    args.push(quote_systemd_arg("server"));
    args.join(" ")
}

fn absolutize_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    Ok(cwd.join(path))
}

fn quote_systemd_arg(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}
