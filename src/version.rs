pub struct BuildInfo {
    pub pkg_version: &'static str,
    pub git_sha: &'static str,
    pub git_sha_short: &'static str,
    pub git_branch: &'static str,
    pub git_describe: &'static str,
    pub git_dirty: &'static str,
    pub build_time: &'static str,
    pub build_unix: &'static str,
    pub rustc: &'static str,
    pub target: &'static str,
    pub host: &'static str,
    pub profile: &'static str,
    pub opt_level: &'static str,
    pub debug: &'static str,
    pub features: &'static str,
}

pub const BUILD_INFO: BuildInfo = BuildInfo {
    pkg_version: env!("CARGO_PKG_VERSION"),
    git_sha: env!("IROH_PROXY_GIT_SHA"),
    git_sha_short: env!("IROH_PROXY_GIT_SHA_SHORT"),
    git_branch: env!("IROH_PROXY_GIT_BRANCH"),
    git_describe: env!("IROH_PROXY_GIT_DESCRIBE"),
    git_dirty: env!("IROH_PROXY_GIT_DIRTY"),
    build_time: env!("IROH_PROXY_BUILD_TIME"),
    build_unix: env!("IROH_PROXY_BUILD_UNIX"),
    rustc: env!("IROH_PROXY_RUSTC"),
    target: env!("IROH_PROXY_TARGET"),
    host: env!("IROH_PROXY_HOST"),
    profile: env!("IROH_PROXY_PROFILE"),
    opt_level: env!("IROH_PROXY_OPT_LEVEL"),
    debug: env!("IROH_PROXY_DEBUG"),
    features: env!("IROH_PROXY_FEATURES"),
};

pub fn short_version() -> String {
    let dirty = if BUILD_INFO.git_dirty == "true" {
        "-dirty"
    } else {
        ""
    };
    format!(
        "iroh-proxy {} ({}{})",
        BUILD_INFO.pkg_version, BUILD_INFO.git_sha_short, dirty
    )
}

pub fn print_detailed() {
    let bi = &BUILD_INFO;
    println!("iroh-proxy {}", bi.pkg_version);
    println!();
    println!("git:");
    println!("  commit:   {}", bi.git_sha);
    println!("  short:    {}", bi.git_sha_short);
    println!("  branch:   {}", bi.git_branch);
    println!("  describe: {}", bi.git_describe);
    println!("  dirty:    {}", bi.git_dirty);
    println!();
    println!("build:");
    println!("  time:      {} ({})", bi.build_time, bi.build_unix);
    println!("  profile:   {}", bi.profile);
    println!("  opt-level: {}", bi.opt_level);
    println!("  debug:     {}", bi.debug);
    println!("  features:  {}", bi.features);
    println!();
    println!("toolchain:");
    println!("  rustc:  {}", bi.rustc);
    println!("  target: {}", bi.target);
    println!("  host:   {}", bi.host);
}
