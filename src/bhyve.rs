use crate::config::{Config, BASE_PATH, VM_BASE};
use crate::net;
use crate::util::cmd;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VmConfig {
    pub name: String,
    pub disk: PathBuf,
    pub iso: Option<PathBuf>,
    pub vnc_port: Option<u16>,
    pub vnc_bind: String,
    pub vnc_width: u32,
    pub vnc_height: u32,
    pub tpm: bool,
    pub cpus: u32,
    pub memory: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

pub struct VmCreateOptions<'a> {
    pub disk: &'a str,
    pub iso: Option<&'a str>,
    pub vnc_port: Option<u16>,
    pub vnc_bind: &'a str,
    pub vnc_width: u32,
    pub vnc_height: u32,
    pub tpm: bool,
    pub cpus: u32,
    pub memory: &'a str,
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        bail!("invalid VM name: {}", name);
    }
    Ok(())
}

fn vm_dir(name: &str, _config: &Config) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(Path::new(VM_BASE).join(name))
}

fn config_path(name: &str, config: &Config) -> Result<PathBuf> {
    Ok(vm_dir(name, config)?.join("config.toml"))
}

fn read_config(name: &str, config: &Config) -> Result<VmConfig> {
    let path = config_path(name, config)?;
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read VM configuration {}", path.display()))?;
    let mut vm: VmConfig = toml::from_str(&text)
        .with_context(|| format!("failed to parse VM configuration {}", path.display()))?;
    vm.name = name.to_string();
    Ok(vm)
}

pub fn create(name: &str, options: &VmCreateOptions<'_>, config: &Config) -> Result<()> {
    let dir = vm_dir(name, config)?;
    if dir.exists() {
        bail!("VM already exists: {}", name);
    }
    let jail_dir = Path::new(BASE_PATH).join("jail").join(name);
    if jail_dir.exists() {
        bail!(
            "a jail or jail configuration with this name already exists: {}",
            name
        );
    }
    if options.cpus == 0 || options.cpus > 256 {
        bail!("VM CPU count must be between 1 and 256");
    }
    if options.memory.trim().is_empty() {
        bail!("VM memory must not be empty");
    }
    if options.vnc_port == Some(0) || options.vnc_width == 0 || options.vnc_height == 0 {
        bail!("VNC port, width, and height must be greater than zero");
    }
    if options.vnc_port.is_some() && options.vnc_bind.trim().is_empty() {
        bail!("VNC bind address must not be empty");
    }
    let disk_dataset = format!("{}{}/disk.vol", config.zfs_pool, dir.display());
    let disk_path = PathBuf::from(format!("/dev/zvol/{disk_dataset}"));
    let iso_path = options
        .iso
        .map(|path| {
            Path::new(path)
                .canonicalize()
                .with_context(|| format!("ISO does not exist: {}", path))
        })
        .transpose()?;
    let vm_dataset = format!("{}{}", config.zfs_pool, dir.display());
    cmd::run("zfs", &["create", "-p", &vm_dataset])?;
    cmd::run("zfs", &["create", "-V", options.disk, &disk_dataset])?;
    fs::write(
        dir.join("config.toml"),
        toml::to_string(&VmConfig {
            name: name.to_string(),
            disk: disk_path,
            iso: iso_path,
            vnc_port: options.vnc_port,
            vnc_bind: options.vnc_bind.trim().to_string(),
            vnc_width: options.vnc_width,
            vnc_height: options.vnc_height,
            tpm: options.tpm,
            cpus: options.cpus,
            memory: options.memory.trim().to_string(),
            dependencies: Vec::new(),
            enabled: true,
        })?,
    )?;
    Ok(())
}

pub fn set_dependencies(name: &str, dependencies: Vec<String>, config: &Config) -> Result<()> {
    crate::dependency::validate(name, &dependencies)?;
    let path = config_path(name, config)?;
    let text = fs::read_to_string(&path)?;
    let mut vm: VmConfig = toml::from_str(&text)?;
    vm.dependencies = dependencies;
    fs::write(path, toml::to_string(&vm)?)?;
    Ok(())
}

pub fn set_enabled(name: &str, enabled: bool, config: &Config) -> Result<()> {
    let path = config_path(name, config)?;
    crate::dependency::set_enabled(&path, enabled)
}

pub fn path_exists(name: &str, config: &Config) -> bool {
    vm_dir(name, config)
        .map(|path| path.exists())
        .unwrap_or(false)
}

fn pid_path(name: &str, config: &Config) -> Result<PathBuf> {
    Ok(vm_dir(name, config)?.join("bhyve.pid"))
}

fn tpm_pid_path(name: &str, config: &Config) -> Result<PathBuf> {
    Ok(vm_dir(name, config)?.join("tpm.pid"))
}

fn tap_path(name: &str, config: &Config) -> Result<PathBuf> {
    Ok(vm_dir(name, config)?.join("tap"))
}

fn tpm_socket_path(name: &str) -> Result<(String, String)> {
    validate_name(name)?;
    let tpm_dir = Path::new("/var/run/swtpm");
    let socket = tpm_dir.join(name);
    if socket.exists() {
        bail!("TPM socket already exists: {}", socket.display());
    }
    Ok((tpm_dir.display().to_string(), socket.display().to_string()))
}

fn configure_tap(bridge: &str) -> Result<String> {
    let actual = net::create("tap")?;
    if actual.is_empty() {
        bail!("tap creation returned an empty interface name");
    }
    net::set_up(&actual)?;
    net::bridge_add(bridge, &actual)?;
    Ok(actual)
}

fn destroy_tap(tap: &str, bridge: &str) {
    let _ = net::bridge_delete(bridge, tap);
    let _ = net::destroy(tap);
}

pub fn start(name: &str, config: &Config) -> Result<()> {
    start_with_stack(name, config, &mut HashSet::new())
}

pub(crate) fn start_with_stack(
    name: &str,
    config: &Config,
    stack: &mut HashSet<String>,
) -> Result<()> {
    if is_running(name, config) {
        return Ok(());
    }
    if !stack.insert(name.to_string()) {
        bail!("cyclic resource dependency involving {name}");
    }
    let vm = read_config(name, config)?;
    for dependency in vm.dependencies {
        if crate::jail::path_exists(&dependency, config) {
            crate::jail::start_with_stack(&dependency, config, stack)?;
        } else if path_exists(&dependency, config) {
            start_with_stack(&dependency, config, stack)?;
        } else {
            bail!("dependency does not exist: {dependency}");
        }
    }
    let result = start_inner(name, config);
    stack.remove(name);
    result
}

fn start_inner(name: &str, config: &Config) -> Result<()> {
    let vm = read_config(name, config)?;
    if is_running(name, config) {
        bail!("VM is already running: {}", name);
    }
    let (tpm_socket, mut tpm_child) = if vm.tpm {
        let (tpm_dir, actual) = tpm_socket_path(name)?;
        let command = format!(
            "swtpm socket --tpmstate dir={} --tpm2 --ctrl type=unixio,path={}",
            tpm_dir, actual
        );
        let child = Command::new("/bin/sh")
            .args(["-c", &format!("exec {}", command)])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to start swtpm")?;
        (Some(actual), Some(child))
    } else {
        (None, None)
    };

    let tap = match configure_tap(&config.bridge) {
        Ok(tap) => tap,
        Err(error) => {
            if let Some(mut child) = tpm_child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(error);
        }
    };
    let tap_file = tap_path(name, config)?;
    if let Err(error) = fs::write(&tap_file, &tap) {
        destroy_tap(&tap, &config.bridge);
        if let Some(mut child) = tpm_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        return Err(error.into());
    }
    let mut args = vec![
        "-c".to_string(),
        vm.cpus.to_string(),
        "-m".to_string(),
        vm.memory.clone(),
        "-A".to_string(),
        "-H".to_string(),
        "-P".to_string(),
        "-s".to_string(),
        "0,hostbridge".to_string(),
        "-s".to_string(),
        format!("3,nvme,{}", vm.disk.display()),
        "-s".to_string(),
        format!("2,virtio-net,{}", tap),
    ];
    if let Some(iso) = vm.iso {
        args.extend(["-s".to_string(), format!("4,ahci-cd,{}", iso.display())]);
    }
    if let Some(port) = vm.vnc_port {
        args.extend([
            "-s".to_string(),
            format!(
                "29,fbuf,tcp={}:{},w={},h={}",
                vm.vnc_bind, port, vm.vnc_width, vm.vnc_height
            ),
        ]);
    }
    if let Some(socket) = tpm_socket {
        args.extend(["-s".to_string(), format!("31,lpc,tpm,path={}", socket)]);
    }
    if config.vm_firmware.exists() {
        args.extend([
            "-l".to_string(),
            format!("bootrom,{}", config.vm_firmware.display()),
        ]);
    }
    args.push(vm.name.clone());
    let child = match Command::new("bhyve")
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            destroy_tap(&tap, &config.bridge);
            let _ = fs::remove_file(&tap_file);
            if let Some(mut tpm) = tpm_child.take() {
                let _ = tpm.kill();
                let _ = tpm.wait();
            }
            return Err(error)
                .context("failed to start bhyve; is bhyve installed and virtualization enabled?");
        }
    };
    let bhyve_pid = pid_path(name, config)?;
    let tpm_pid = tpm_pid_path(name, config)?;
    fs::write(&bhyve_pid, child.id().to_string())?;
    if let Some(tpm) = tpm_child.as_ref() {
        fs::write(&tpm_pid, tpm.id().to_string())?;
    }
    let bridge = config.bridge.clone();
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
        if let Some(mut tpm) = tpm_child {
            let _ = tpm.kill();
            let _ = tpm.wait();
        }
        destroy_tap(&tap, &bridge);
        let _ = fs::remove_file(bhyve_pid);
        let _ = fs::remove_file(tpm_pid);
        let _ = fs::remove_file(tap_file);
    });
    Ok(())
}

pub fn stop(name: &str, config: &Config) -> Result<()> {
    let _ = read_config(name, config)?;
    if is_running(name, config) {
        let _ = cmd::run("bhyvectl", &["--vm", name, "--destroy"]);
        if let Ok(pid) = fs::read_to_string(pid_path(name, config)?) {
            let _ = cmd::run("kill", &["-TERM", pid.trim()]);
        }
    }
    if let Ok(pid) = fs::read_to_string(tpm_pid_path(name, config)?) {
        let _ = cmd::run("kill", &["-TERM", pid.trim()]);
    }
    if let Ok(tap) = fs::read_to_string(tap_path(name, config)?) {
        destroy_tap(tap.trim(), &config.bridge);
    }
    let _ = fs::remove_file(pid_path(name, config)?);
    let _ = fs::remove_file(tpm_pid_path(name, config)?);
    let _ = fs::remove_file(tap_path(name, config)?);
    Ok(())
}

pub fn destroy(name: &str, config: &Config) -> Result<()> {
    let dir = vm_dir(name, config)?;
    if !dir.exists() {
        bail!("VM does not exist: {}", name);
    }
    stop(name, config)?;
    crate::dependency::remove_references(name)?;
    let dataset = format!("{}{}", config.zfs_pool, dir.display());
    cmd::run("zfs", &["destroy", "-r", "-f", &dataset])?;
    Ok(())
}

pub fn is_running(name: &str, config: &Config) -> bool {
    let Ok(pid) = fs::read_to_string(pid_path(name, config).unwrap_or_default()) else {
        return false;
    };
    Command::new("kill")
        .args(["-0", pid.trim()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn list(config: &Config) -> Result<Vec<(String, bool)>> {
    let mut result = Vec::new();
    let vm_base = Path::new(VM_BASE);
    if !vm_base.exists() {
        return Ok(result);
    }
    for entry in fs::read_dir(vm_base)? {
        let entry = entry?;
        if entry.path().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if config_path(&name, config)?.exists() {
                result.push((name.clone(), is_running(&name, config)));
            }
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}
