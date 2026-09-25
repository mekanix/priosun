use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

macro_rules! base_path {
    () => {
        "/var/priosun"
    };
}

pub const BASE_PATH: &str = base_path!();
pub const BASE_DATASET_PATH: &str = concat!(base_path!(), "/base");
pub const JAIL_BASE: &str = concat!(base_path!(), "/jail");
pub const VM_BASE: &str = concat!(base_path!(), "/vm");
pub const SEED_BASE: &str = concat!(base_path!(), "/seed");
pub const IMAGE_BASE: &str = concat!(base_path!(), "/images");
pub const LOG_BASE: &str = "/var/log/priosun";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Config {
    pub version: String,
    pub master_ip6: String,
    pub bridge: String,
    pub bridge_ip: String,
    pub bridge_ip6: String,
    pub use_ipv4: bool,
    pub use_ipv6: bool,
    pub network_ip: String,
    pub network_ip6: String,
    pub domain: String,
    pub zfs_pool: String,
    pub pkg_mirror: String,
    pub pkg_repo: String,
    pub pkg_proxy: String,
    pub ipv6_prefix: String,
    pub dhcp: String,
    pub projects_dir: Option<PathBuf>,
    pub bridge_members: Vec<String>,
    pub dns_override: Vec<String>,
    pub resolv_conf: PathBuf,
    pub pkg_repos: Vec<String>,
    pub allow: Vec<String>,
    pub prestart: Option<String>,
    pub poststart: Option<String>,
    pub prestop: Option<String>,
    pub poststop: Option<String>,
    pub vm_firmware: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: "0.1.0".to_string(),
            master_ip6: ":2".to_string(),
            bridge: "jails".to_string(),
            bridge_ip: "172.16.0.254".to_string(),
            bridge_ip6: ":1".to_string(),
            use_ipv4: true,
            use_ipv6: true,
            network_ip: "172.16.0.253".to_string(),
            network_ip6: ":2".to_string(),
            domain: String::new(),
            zfs_pool: String::new(),
            pkg_mirror: "pkg.FreeBSD.org".to_string(),
            pkg_repo: "latest".to_string(),
            pkg_proxy: "no".to_string(),
            ipv6_prefix: "fd10:6c79:8ae5:8b91:".to_string(),
            dhcp: "dhcpcd".to_string(),
            projects_dir: None,
            bridge_members: Vec::new(),
            dns_override: Vec::new(),
            resolv_conf: PathBuf::from("/etc/resolv.conf"),
            pkg_repos: Vec::new(),
            allow: Vec::new(),
            prestart: None,
            poststart: None,
            prestop: None,
            poststop: None,
            vm_firmware: PathBuf::from("/usr/local/share/uefi-firmware/BHYVE_UEFI.fd"),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let conf_path = std::env::args()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|args| args[0] == "--config")
            .map(|args| PathBuf::from(&args[1]))
            .unwrap_or_else(|| PathBuf::from("/etc/local/etc/priosun.toml"));
        let mut config = if conf_path.exists() {
            let content = std::fs::read_to_string(&conf_path)
                .with_context(|| format!("failed to read {}", conf_path.display()))?;
            toml::from_str::<Self>(&content)
                .with_context(|| format!("failed to parse {} as TOML", conf_path.display()))
        } else {
            Ok(Self::default())
        }?;
        if config.zfs_pool.trim().is_empty() {
            config.zfs_pool = discover_zfs_pool()?;
        }
        Ok(config)
    }
}

fn discover_zfs_pool() -> Result<String> {
    let output = std::process::Command::new("zpool")
        .args(["list", "-H"])
        .output()
        .context("failed to execute zpool list -H")?;
    if !output.status.success() {
        bail!(
            "zpool list -H failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pools = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect::<Vec<_>>();
    match pools.as_slice() {
        [pool] => Ok((*pool).to_string()),
        [] => bail!("no ZFS pools are available"),
        _ => bail!("zfs_pool is not set and multiple ZFS pools are available"),
    }
}
