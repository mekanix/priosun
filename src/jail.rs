use crate::bhyve;
use crate::config::{Config, BASE_DATASET_PATH, JAIL_BASE, LOG_BASE};
use crate::net;
use crate::template;
use crate::util::cmd;
use anyhow::{bail, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

pub fn check(_name: &str, chroot: &Path) -> Result<()> {
    if chroot.exists() {
        bail!("{} already exists!", chroot.display());
    }
    Ok(())
}

fn qualified_hostname(name: &str) -> Result<String> {
    let output = Command::new("hostname").output()?;
    if !output.status.success() {
        bail!("failed to read system hostname");
    }
    let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if host.is_empty() {
        bail!("system hostname is empty");
    }
    Ok(format!("{name}.{host}"))
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        bail!("invalid jail name: {}", name);
    }
    Ok(())
}

pub fn create(
    name: &str,
    set: Option<&str>,
    version: Option<&str>,
    base: bool,
    from_base: Option<&str>,
    ssh_key: Option<&str>,
    config: &Config,
) -> Result<()> {
    let parsed_version = version.map(parse_version).transpose()?;
    cmd::message(&format!("Creating jail {name}"));
    let hostname = qualified_hostname(name)?;
    let jail_dir = Path::new(JAIL_BASE).join(name);
    let base_dir = Path::new(BASE_DATASET_PATH).join(name);
    let root_dir = if base { &base_dir } else { &jail_dir };
    check(name, root_dir)?;
    if !base && base_dir.exists() {
        bail!("a base jail with this name already exists: {}", name);
    }
    if bhyve::path_exists(name, config) {
        bail!(
            "a VM or VM directory with this name already exists: {}",
            name
        );
    }
    if let Some(base_name) = from_base {
        validate_name(base_name)?;
        let source = Path::new(BASE_DATASET_PATH).join(base_name);
        if !source.exists() {
            bail!("base jail does not exist: {}", base_name);
        }
    }
    // Always create the jail root as a ZFS dataset.
    let chroot = if base {
        base_dir.clone()
    } else {
        jail_dir.clone()
    };
    if let Some(base_name) = from_base {
        let source_path = Path::new(BASE_DATASET_PATH).join(base_name);
        let source = format!("{}{}@base", config.zfs_pool, source_path.display());
        let target = format!("{}{}", config.zfs_pool, jail_dir.display());
        cmd::message("Cloning the base jail ZFS dataset");
        cmd::run("zfs", &["clone", &source, &target])?;
    } else {
        let zfs_path = if base {
            format!("{}{}", config.zfs_pool, base_dir.display())
        } else {
            format!("{}{}", config.zfs_pool, jail_dir.display())
        };
        cmd::message("Creating the jail ZFS dataset");
        cmd::run("zfs", &["create", "-p", &zfs_path])?;
    }
    fs::create_dir_all(&chroot)?;
    let selected_set = set.unwrap_or("FreeBSD-set-base-jail");
    if !base {
        let dataset = format!("{}{}", config.zfs_pool, jail_dir.display());
        crate::metadata::set(&dataset, "enabled", "true")?;
        crate::metadata::set(&dataset, "dependencies", "")?;
        crate::metadata::set(&dataset, "provisioned", "false")?;
    }

    // Set up pkg repos
    cmd::message("Configuring package repositories");
    let pkg_repos = chroot.join("usr/local/etc/pkg/repos");
    fs::create_dir_all(&pkg_repos)?;
    let base_repo = parsed_version
        .as_ref()
        .map(|(_, minor, _)| format!("base_release_{minor}"))
        .unwrap_or_else(|| "base_release_${VERSION_MINOR}".to_string());
    fs::write(
        pkg_repos.join("FreeBSD.conf"),
        format!(
            "FreeBSD-ports: {{ url: \"pkg+https://pkg.FreeBSD.org/${{ABI}}/latest\" }}\nFreeBSD-base: {{\n  url: \"pkg+https://pkg.FreeBSD.org/${{ABI}}/{base_repo}\",\n  enabled: yes\n}}\n\n"
        ),
    )?;

    // Copy keys
    cmd::message("Installing trusted package keys");
    let keys_src = Path::new("/usr/share/keys");
    let keys_dst = chroot.join("usr/share/keys");
    if keys_src.exists() {
        fs::create_dir_all(&keys_dst)?;
        for entry in fs::read_dir(keys_src)? {
            let entry = entry?;
            let src = entry.path();
            let dst = keys_dst.join(entry.file_name());
            if src.is_dir() {
                copy_dir_all(&src, &dst)?;
            } else {
                fs::copy(&src, &dst)?;
            }
        }
    }

    // Mount devfs
    cmd::message("Mounting devfs for jail initialization");
    let dev_dir = chroot.join("dev");
    fs::create_dir_all(&dev_dir)?;
    cmd::run(
        "mount",
        &["-t", "devfs", "devfs", &dev_dir.display().to_string()],
    )?;

    cmd::message(&format!("Installing {selected_set} and dhcpcd"));
    let chroot_path = chroot.display().to_string();
    let mut pkg_args = vec!["-r".to_string(), chroot_path];
    if let Some((major, _, osreldate)) = parsed_version {
        let arch = String::from_utf8_lossy(&cmd::run("uname", &["-p"])?.stdout)
            .trim()
            .to_string();
        pkg_args.extend([
            "-o".to_string(),
            format!("ABI=FreeBSD:{major}:{arch}"),
            "-o".to_string(),
            format!("OSVERSION={osreldate}"),
        ]);
    }
    pkg_args.extend([
        "install".to_string(),
        "-y".to_string(),
        selected_set.to_string(),
        "dhcpcd".to_string(),
    ]);
    let pkg_args = pkg_args.iter().map(String::as_str).collect::<Vec<_>>();
    cmd::run("pkg", &pkg_args)?;

    if !base {
        cmd::message("Creating the provision user");
        let chroot_path = chroot.display().to_string();
        let user_args = [
            "-R",
            chroot_path.as_str(),
            "useradd",
            "provision",
            "-m",
            "-s",
            "/bin/sh",
            "-G",
            "wheel",
            "-h",
            "0",
        ];
        cmd::run_with_stdin("pw", &user_args, b"provision\n")?;
        cmd::message("Setting a random root password");
        let root_args = ["-R", chroot_path.as_str(), "usermod", "root", "-h", "0"];
        let root_password = crate::util::random_password()?;
        let root_input = format!("{root_password}\n");
        cmd::run_with_stdin("pw", &root_args, root_input.as_bytes())?;
        if let Some(ssh_key) = ssh_key {
            install_ssh_key(&chroot, ssh_key)?;
        }
    }

    // Copy resolv.conf
    cmd::message("Configuring DNS");
    fs::copy("/etc/resolv.conf", chroot.join("etc/resolv.conf"))?;

    if config.pkg_proxy != "no" {
        let pkg_conf = chroot.join("usr/local/etc/pkg.conf");
        let proxy_line = format!(
            "pkg_env : {{ http_proxy: \"http://{}/\" }}\n",
            config.pkg_proxy
        );
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&pkg_conf)?;
        std::io::Write::write_all(&mut f, proxy_line.as_bytes())?;
    }

    // Hostname and services
    cmd::message("Configuring hostname and jail services");
    cmd::run(
        "sysrc",
        &[
            "-R",
            &chroot.display().to_string(),
            &format!("hostname={hostname}"),
        ],
    )?;
    cmd::run(
        "sysrc",
        &["-R", &chroot.display().to_string(), "sshd_enable=YES"],
    )?;
    cmd::run(
        "sysrc",
        &["-R", &chroot.display().to_string(), "clear_tmp_enable=YES"],
    )?;

    // MAC do rules
    cmd::message("Configuring MAC rules");
    let sysctl_conf = chroot.join("etc/sysctl.conf");
    let mac_rule = "security.mac.do.rules=gid=0:any\n";
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&sysctl_conf)?;
    std::io::Write::write_all(&mut f, mac_rule.as_bytes())?;

    // resolv.conf
    let mut resolv = String::new();
    if !config.dns_override.is_empty() {
        for ns in &config.dns_override {
            let include = match ns.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(_)) => config.use_ipv4,
                Ok(std::net::IpAddr::V6(_)) => config.use_ipv6,
                Err(_) => true,
            };
            if include {
                resolv.push_str(&format!("nameserver {}\n", ns));
            }
        }
    } else {
        if config.use_ipv4 {
            resolv.push_str(&format!("nameserver {}\n", config.bridge_ip));
        }
        if config.use_ipv6 {
            resolv.push_str(&format!(
                "nameserver {}{}\n",
                config.ipv6_prefix, config.bridge_ip6
            ));
        }
    }
    fs::write(chroot.join("etc/resolv.conf"), resolv)?;

    // Network config
    cmd::message("Configuring jail networking");
    if config.dhcp == "dhcpcd" {
        let rc_conf = chroot.join("etc/rc.conf");
        let mut f = fs::OpenOptions::new().append(true).open(&rc_conf)?;
        std::io::Write::write_all(
            &mut f,
            "dhclient_program=\"/usr/local/sbin/dhcpcd\"\n".as_bytes(),
        )?;
        let dhcpcd_conf = chroot.join("usr/local/etc/dhcpcd.conf");
        if let Some(parent) = dhcpcd_conf.parent() {
            fs::create_dir_all(parent)?;
        }
        if dhcpcd_conf.exists() {
            let content = fs::read_to_string(&dhcpcd_conf)?;
            let content = content.replace("#hostname", "hostname");
            fs::write(&dhcpcd_conf, content)?;
        }
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&dhcpcd_conf)?;
        std::io::Write::write_all(&mut f, "ipv6ra_noautoconf\n".as_bytes())?;
        cmd::run(
            "sysrc",
            &[
                "-R",
                &chroot.display().to_string(),
                "ifconfig_eth0=SYNCDHCP",
            ],
        )?;
    } else {
        cmd::run(
            "sysrc",
            &[
                "-R",
                &chroot.display().to_string(),
                "ifconfig_eth0=SYNCDHCP",
            ],
        )?;
        cmd::run(
            "sysrc",
            &[
                "-R",
                &chroot.display().to_string(),
                "ifconfig_eth0_ipv6=inet6 -ifdisabled accept_rtadv auto_linklocal",
            ],
        )?;
    }

    // PF in jail
    cmd::message("Configuring PF");
    let rc_conf = chroot.join("etc/rc.conf");
    let mut f = fs::OpenOptions::new().append(true).open(&rc_conf)?;
    std::io::Write::write_all(
        &mut f,
        "pf_enable=\"YES\"\npflog_enable=\"YES\"\n".as_bytes(),
    )?;
    let pf_jail = template::template_dir().join("pf-jail.conf");
    fs::copy(&pf_jail, chroot.join("etc/pf.conf"))?;
    fs::write(chroot.join("etc/pf.services"), "")?;

    // Unmount devfs only after the jail has been fully initialized.
    cmd::message("Unmounting devfs and finishing jail initialization");
    cmd::run("umount", &[&dev_dir.display().to_string()])?;

    if base {
        let dataset = format!("{}{}@base", config.zfs_pool, base_dir.display());
        cmd::message("Creating the base jail snapshot");
        cmd::run("zfs", &["snapshot", &dataset])?;
    }

    Ok(())
}

fn install_ssh_key(chroot: &Path, ssh_key: &str) -> Result<()> {
    if ssh_key.contains(['\n', '\r']) || ssh_key.trim().is_empty() {
        bail!("SSH public key must be a single non-empty line");
    }
    let ssh_dir = chroot.join("home/provision/.ssh");
    fs::create_dir_all(&ssh_dir)?;
    fs::write(ssh_dir.join("authorized_keys"), format!("{ssh_key}\n"))?;
    let ssh_dir_path = ssh_dir.display().to_string();
    cmd::run("chown", &["-R", "provision:provision", &ssh_dir_path])?;
    cmd::run("chmod", &["700", &ssh_dir_path])?;
    Ok(())
}

fn parse_version(version: &str) -> Result<(u32, u32, i32)> {
    let (major, minor) = version.split_once('.').ok_or_else(|| {
        anyhow::anyhow!("invalid FreeBSD version {version:?}; expected MAJOR.MINOR")
    })?;
    let major = major
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("invalid FreeBSD major version: {major}"))?;
    let minor = minor
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("invalid FreeBSD minor version: {minor}"))?;
    if minor > 99 {
        bail!("invalid FreeBSD minor version: {minor}");
    }
    let osreldate = major
        .checked_mul(100_000)
        .and_then(|value| value.checked_add(minor * 1_000))
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| anyhow::anyhow!("FreeBSD version is too large: {version}"))?;
    Ok((major, minor, osreldate))
}

pub fn set_dependencies(name: &str, dependencies: Vec<String>, config: &Config) -> Result<()> {
    crate::dependency::validate(name, &dependencies, config)?;
    let dataset = format!(
        "{}{}",
        config.zfs_pool,
        Path::new(JAIL_BASE).join(name).display()
    );
    crate::metadata::set(&dataset, "dependencies", &dependencies.join(","))?;
    Ok(())
}

pub fn set_enabled(name: &str, enabled: bool, config: &Config) -> Result<()> {
    let dataset = format!(
        "{}{}",
        config.zfs_pool,
        Path::new(JAIL_BASE).join(name).display()
    );
    crate::metadata::set(&dataset, "enabled", &enabled.to_string())
}

pub fn destroy(name: &str, config: &Config) -> Result<()> {
    let pool = &config.zfs_pool;

    stop(name, config)?;
    crate::dependency::remove_references(name, config)?;

    let jail_dir = Path::new(JAIL_BASE).join(name);
    if jail_dir.exists() {
        let zfs_path = format!("{}{}", pool, jail_dir.display());
        cmd::run("zfs", &["destroy", "-r", "-f", &zfs_path])?;
    }
    if jail_dir.exists() {
        fs::remove_dir(jail_dir)?;
    }

    Ok(())
}

pub fn destroy_base(name: &str, config: &Config) -> Result<()> {
    let base_dir = Path::new(BASE_DATASET_PATH).join(name);
    if !base_dir.exists() {
        bail!("base jail does not exist: {}", name);
    }
    let dataset = format!("{}{}", config.zfs_pool, base_dir.display());
    cmd::run("zfs", &["destroy", "-r", "-f", &dataset])?;
    if base_dir.exists() {
        fs::remove_dir_all(base_dir)?;
    }
    Ok(())
}

pub fn path_exists(name: &str, _config: &Config) -> bool {
    let path = Path::new(JAIL_BASE).join(name);
    path.is_dir()
}

#[derive(Debug, Clone)]
pub struct JailInfo {
    pub name: String,
    pub hostname: String,
    pub ips: String,
    pub status: String,
}

pub fn list(_config: &Config) -> Result<Vec<JailInfo>> {
    let mut output = Vec::new();
    let jail_base = Path::new(JAIL_BASE);
    if !jail_base.exists() {
        return Ok(output);
    }
    for entry in fs::read_dir(jail_base)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let (hostname, ips, status) = match ::jail::RunningJail::from_name(&name) {
            Ok(jail) => {
                let hostname = jail.hostname()?;
                let ips = jail
                    .ips()?
                    .into_iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                (hostname, ips, "running".to_string())
            }
            Err(::jail::JailError::JailGetError(_)) => {
                (String::new(), String::new(), "stopped".to_string())
            }
            Err(error) => return Err(anyhow::anyhow!("failed to find jail {name}: {error}")),
        };
        output.push(JailInfo {
            name,
            hostname,
            ips,
            status,
        });
    }
    output.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(output)
}

fn dependencies(name: &str, config: &Config) -> Result<Vec<String>> {
    let dataset = format!(
        "{}{}",
        config.zfs_pool,
        Path::new(JAIL_BASE).join(name).display()
    );
    Ok(crate::metadata::get(&dataset, "dependencies")?
        .unwrap_or_default()
        .split(',')
        .filter(|dependency| !dependency.is_empty())
        .map(str::to_string)
        .collect())
}

fn root_path(name: &str, _config: &Config) -> PathBuf {
    Path::new(JAIL_BASE).join(name)
}

pub fn start(name: &str, config: &Config) -> Result<()> {
    start_with_stack(name, config, &mut HashSet::new())
}

pub(crate) fn start_with_stack(
    name: &str,
    config: &Config,
    stack: &mut HashSet<String>,
) -> Result<()> {
    if ::jail::RunningJail::from_name(name).is_ok() {
        return Ok(());
    }
    if !stack.insert(name.to_string()) {
        bail!("cyclic resource dependency involving {name}");
    }
    for dependency in dependencies(name, config)? {
        if path_exists(&dependency, config) {
            start_with_stack(&dependency, config, stack)?;
        } else if bhyve::path_exists(&dependency, config) {
            bhyve::start_with_stack(&dependency, config, stack)?;
        } else {
            bail!("dependency does not exist: {dependency}");
        }
    }
    let result = start_inner(name, config);
    stack.remove(name);
    result
}

fn start_inner(name: &str, config: &Config) -> Result<()> {
    let hostname = qualified_hostname(name)?;
    cmd::message(&format!("Creating a network interface for jail {name}"));
    let group = name
        .trim_end_matches(|character: char| character.is_ascii_digit())
        .chars()
        .take(15)
        .collect::<String>();
    let epair_a = net::create("epair")?;
    if !epair_a.starts_with("epair") || !epair_a.ends_with('a') {
        bail!("epair creation returned an invalid interface: {epair_a}");
    }
    let epair_b = format!("{}b", &epair_a[..epair_a.len() - 1]);
    let setup = if group.is_empty() {
        net::set_up(&epair_a)
    } else {
        net::group_add(&epair_a, &group).and_then(|_| net::set_up(&epair_a))
    };
    if let Err(error) = setup {
        let _ = net::destroy(&epair_a);
        return Err(error);
    }

    cmd::message(&format!("Adding {epair_a} to bridge {}", config.bridge));
    if let Err(error) = net::bridge_add(&config.bridge, &epair_a) {
        let _ = net::destroy(&epair_a);
        return Err(error);
    }

    let dev_dir = root_path(name, config).join("dev");
    cmd::message(&format!("Mounting devfs at {}", dev_dir.display()));
    if let Err(error) = cmd::run(
        "mount",
        &["-t", "devfs", "devfs", &dev_dir.display().to_string()],
    ) {
        let _ = net::bridge_delete(&config.bridge, &epair_a);
        let _ = net::destroy(&epair_a);
        return Err(error);
    }

    cmd::message(&format!("Starting jail {name} with {epair_b}"));
    // vnet.interface is a jail(8) convenience option, not a kernel jail
    // parameter. Store the host-side epair in the kernel's string-valued
    // domainname field so it can be recovered when the jail is stopped.
    let stopped = ::jail::StoppedJail::new(root_path(name, config))
        .name(name)
        .hostname(hostname)
        .param("vnet", ::jail::param::Value::Int(1))
        .param(
            "host.domainname",
            ::jail::param::Value::String(epair_a.clone()),
        );
    let running = match stopped
        .start()
        .map_err(|error| anyhow::anyhow!("failed to start jail {name}: {error}"))
    {
        Ok(running) => running,
        Err(error) => {
            let _ = cmd::run("umount", &[&dev_dir.display().to_string()]);
            let _ = net::bridge_delete(&config.bridge, &epair_a);
            let _ = net::destroy(&epair_a);
            return Err(error);
        }
    };
    if let Err(error) = net::move_to_vnet(&epair_b, running.jid) {
        let _ = running.stop();
        let _ = cmd::run("umount", &[&dev_dir.display().to_string()]);
        let _ = net::bridge_delete(&config.bridge, &epair_a);
        let _ = net::destroy(&epair_a);
        return Err(error);
    }
    cmd::message(&format!("Renaming {epair_b} to eth0 inside jail {name}"));
    if let Err(error) = net::rename_in_jail(running.jid, &epair_b, "eth0") {
        let _ = cleanup_epair(&epair_a, config);
        let _ = running.stop();
        let _ = cmd::run("umount", &[&dev_dir.display().to_string()]);
        return Err(error);
    }
    cmd::message(&format!("Starting services inside jail {name}"));
    let log_path = Path::new(LOG_BASE).join(format!("{name}.log"));
    if let Err(error) = cmd::run_append_log("jexec", &[name, "/bin/sh", "/etc/rc"], &log_path) {
        let _ = cleanup_epair(&epair_a, config);
        let _ = running.stop();
        let _ = cmd::run("umount", &[&dev_dir.display().to_string()]);
        return Err(error);
    }
    Ok(())
}

pub fn stop(name: &str, config: &Config) -> Result<()> {
    if !path_exists(name, config) {
        bail!("no managed jail exists with this name: {name}");
    }
    match ::jail::RunningJail::from_name(name) {
        Ok(running) => {
            let epair_a = match running.param("host.domainname") {
                Ok(::jail::param::Value::String(value)) => value,
                Ok(value) => {
                    bail!("jail {name} has an unexpected host.domainname value: {value:?}")
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "failed to read host.domainname for jail {name}: {error}"
                    ));
                }
            };
            cleanup_epair(&epair_a, config)?;
            running
                .stop()
                .map_err(|error| anyhow::anyhow!("failed to stop jail {name}: {error}"))?;
            let dev_dir = root_path(name, config).join("dev");
            cmd::message(&format!("Unmounting devfs at {}", dev_dir.display()));
            unmount_devfs(&dev_dir)?;
            Ok(())
        }
        Err(::jail::JailError::JailGetError(_)) => Ok(()),
        Err(error) => Err(anyhow::anyhow!("failed to find jail {name}: {error}")),
    }
}

fn unmount_devfs(path: &Path) -> Result<()> {
    let path = path.display().to_string();
    let mut last_error = None;
    for attempt in 0..50 {
        let output = Command::new("umount").arg(&path).output()?;
        if output.status.success() {
            return Ok(());
        }
        last_error = Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
        if attempt < 49 {
            thread::sleep(Duration::from_millis(100));
        }
    }
    bail!(
        "umount {} failed: {}",
        path,
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )
}

fn cleanup_epair(epair_a: &str, config: &Config) -> Result<()> {
    cmd::message(&format!("Removing {epair_a} from bridge {}", config.bridge));
    net::bridge_delete(&config.bridge, epair_a)?;
    cmd::message(&format!("Destroying network interface {epair_a}"));
    net::destroy(epair_a)?;
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &dest)?;
        } else {
            fs::copy(&path, &dest)?;
        }
    }
    Ok(())
}
