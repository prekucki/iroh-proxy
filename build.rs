use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn iso8601_utc(secs: u64) -> String {
    // Convert unix seconds to YYYY-MM-DDTHH:MM:SSZ without extra deps.
    let days = (secs / 86_400) as i64;
    let mut remainder = (secs % 86_400) as u32;
    let hour = remainder / 3600;
    remainder %= 3600;
    let minute = remainder / 60;
    let second = remainder % 60;

    // Civil-from-days algorithm by Howard Hinnant.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hour, minute, second
    )
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let git_sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let git_short = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let git_branch =
        git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let git_describe =
        git(&["describe", "--tags", "--always", "--dirty"]).unwrap_or_else(|| git_short.clone());
    let git_dirty = match Command::new("git").args(["status", "--porcelain"]).output() {
        Ok(out) if out.status.success() => !out.stdout.is_empty(),
        _ => false,
    };

    let build_unix = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    let build_time = iso8601_utc(build_unix);

    let rustc_version =
        match Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
            .arg("--version")
            .output()
        {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => "unknown".into(),
        };

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    let host = std::env::var("HOST").unwrap_or_else(|_| "unknown".into());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into());
    let opt_level = std::env::var("OPT_LEVEL").unwrap_or_else(|_| "unknown".into());
    let debug = std::env::var("DEBUG").unwrap_or_else(|_| "unknown".into());
    let features: Vec<String> = std::env::vars()
        .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(|n| n.to_lowercase()))
        .collect();
    let features = if features.is_empty() {
        "(none)".to_string()
    } else {
        features.join(",")
    };

    println!("cargo:rustc-env=IROH_PROXY_GIT_SHA={git_sha}");
    println!("cargo:rustc-env=IROH_PROXY_GIT_SHA_SHORT={git_short}");
    println!("cargo:rustc-env=IROH_PROXY_GIT_BRANCH={git_branch}");
    println!("cargo:rustc-env=IROH_PROXY_GIT_DESCRIBE={git_describe}");
    println!("cargo:rustc-env=IROH_PROXY_GIT_DIRTY={git_dirty}");
    println!("cargo:rustc-env=IROH_PROXY_BUILD_TIME={build_time}");
    println!("cargo:rustc-env=IROH_PROXY_BUILD_UNIX={build_unix}");
    println!("cargo:rustc-env=IROH_PROXY_RUSTC={rustc_version}");
    println!("cargo:rustc-env=IROH_PROXY_TARGET={target}");
    println!("cargo:rustc-env=IROH_PROXY_HOST={host}");
    println!("cargo:rustc-env=IROH_PROXY_PROFILE={profile}");
    println!("cargo:rustc-env=IROH_PROXY_OPT_LEVEL={opt_level}");
    println!("cargo:rustc-env=IROH_PROXY_DEBUG={debug}");
    println!("cargo:rustc-env=IROH_PROXY_FEATURES={features}");
}
