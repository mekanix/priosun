use crate::config::{Config, BASE_PATH, IMAGE_BASE, SEED_BASE, VM_BASE};
use crate::net;
use crate::util::cmd;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const VIRTIO_NET_INTERFACE: &str = "virtio-net0";

fn interface_mac_property(interface: &str) -> String {
    format!("mac_{interface}")
}

fn random_mac_address() -> Result<String> {
    let mut bytes = [0_u8; 6];
    fs::File::open("/dev/urandom")
        .context("failed to open /dev/urandom")?
        .read_exact(&mut bytes)
        .context("failed to read random MAC address")?;
    bytes[0] = (bytes[0] & 0xfc) | 0x02;
    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub name: String,
    pub disk: PathBuf,
    pub os: String,
    pub iso: Option<PathBuf>,
    pub vnc_port: Option<u16>,
    pub vnc_bind: String,
    pub vnc_width: u32,
    pub vnc_height: u32,
    pub tpm: bool,
    pub cpus: u32,
    pub memory: String,
    pub dependencies: Vec<String>,
    pub enabled: bool,
    pub interface_macs: HashMap<String, String>,
}

fn validate_os(os: &str) -> Result<()> {
    if os != "freebsd" && os != "ubuntu" && os != "fedora" && os != "debian" {
        bail!("unsupported VM operating system: {os}");
    }
    Ok(())
}

fn cloud_image_version(os: &str, version: Option<&str>) -> Result<String> {
    let version = match version {
        Some(version) => version.to_string(),
        None if os == "ubuntu" => "resolute".to_string(),
        None if os == "fedora" => "44-1.7".to_string(),
        None if os == "debian" => "13".to_string(),
        None => {
            let output = cmd::run("uname", &["-r"])?;
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .split('-')
                .next()
                .unwrap_or_default()
                .to_string()
        }
    };
    if os == "freebsd" {
        let valid = version.split_once('.').is_some()
            && version
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.');
        if !valid {
            bail!("invalid FreeBSD release version: {version}");
        }
    } else if os == "ubuntu" && ubuntu_codename(&version).is_none() {
        bail!("unsupported Ubuntu release version: {version}");
    } else if os == "fedora" && !fedora_version_is_valid(&version) {
        bail!("invalid Fedora release version: {version}; expected MAJOR-RELEASE");
    } else if os == "debian" && debian_release(&version).is_none() {
        bail!("unsupported Debian release version: {version}");
    }
    Ok(version)
}

fn fedora_version_is_valid(version: &str) -> bool {
    let Some((major, release)) = version.split_once('-') else {
        return false;
    };
    !major.is_empty()
        && major.chars().all(|character| character.is_ascii_digit())
        && !release.is_empty()
        && release
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
}

fn fedora_major_version(version: &str) -> &str {
    version.split_once('-').map_or(version, |(major, _)| major)
}

fn ubuntu_codename(version: &str) -> Option<&'static str> {
    match version.to_ascii_lowercase().as_str() {
        "26.04" | "resolute" => Some("resolute"),
        "24.04" | "noble" => Some("noble"),
        "22.04" | "jammy" => Some("jammy"),
        "20.04" | "focal" => Some("focal"),
        _ => None,
    }
}

fn debian_release(version: &str) -> Option<(&'static str, &'static str)> {
    match version.to_ascii_lowercase().as_str() {
        "13" | "trixie" => Some(("13", "trixie")),
        "12" | "bookworm" => Some(("12", "bookworm")),
        "11" | "bullseye" => Some(("11", "bullseye")),
        _ => None,
    }
}

fn cloud_image_arch(os: &str) -> Result<String> {
    let output = cmd::run("uname", &["-m"])?;
    let host_arch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let arch = if os == "ubuntu" || os == "fedora" || os == "debian" {
        match (os, host_arch.as_str()) {
            ("ubuntu", "amd64" | "x86_64") => "amd64".to_string(),
            ("ubuntu", "arm64" | "aarch64") => "arm64".to_string(),
            ("fedora", "amd64" | "x86_64") => "x86_64".to_string(),
            ("fedora", "arm64" | "aarch64") => "aarch64".to_string(),
            ("debian", "amd64" | "x86_64") => "amd64".to_string(),
            ("debian", "arm64" | "aarch64") => "arm64".to_string(),
            _ => bail!("unsupported {os} host architecture: {host_arch}"),
        }
    } else {
        host_arch
    };
    if arch.is_empty() {
        bail!("uname -m returned an empty architecture");
    }
    Ok(arch)
}

fn provision_cloud_image(os: &str, version: &str, disk: &Path) -> Result<()> {
    let arch = cloud_image_arch(os)?;
    let (filename, url) = if os == "ubuntu" {
        let codename = ubuntu_codename(version)
            .ok_or_else(|| anyhow::anyhow!("unsupported Ubuntu release version: {version}"))?;
        let filename = format!("{codename}-server-cloudimg-{arch}.img");
        let url = format!("https://cloud-images.ubuntu.com/{codename}/current/{filename}");
        (filename, url)
    } else if os == "fedora" {
        let major = fedora_major_version(version);
        let filename = format!("Fedora-Cloud-Base-Generic-{version}.{arch}.qcow2");
        let url = format!(
            "https://download.fedoraproject.org/pub/fedora/linux/releases/{major}/Cloud/{arch}/images/{filename}"
        );
        (filename, url)
    } else if os == "debian" {
        let (major, codename) = debian_release(version)
            .ok_or_else(|| anyhow::anyhow!("unsupported Debian release version: {version}"))?;
        let filename = format!("debian-{major}-generic-{arch}.qcow2");
        let url = format!("https://cloud.debian.org/images/cloud/{codename}/latest/{filename}");
        (filename, url)
    } else {
        let filename = format!("FreeBSD-{version}-RELEASE-{arch}-BASIC-CLOUDINIT-zfs.raw.xz");
        let url = format!(
            "https://download.freebsd.org/releases/VM-IMAGES/{version}-RELEASE/{arch}/Latest/{filename}"
        );
        (filename, url)
    };
    let image = PathBuf::from(IMAGE_BASE).join(&filename);
    if !image.exists() {
        cmd::run("fetch", &["-o", &image.display().to_string(), &url])?;
    }
    let image_path = image.display().to_string();
    if os == "ubuntu" || os == "fedora" || os == "debian" {
        cmd::run(
            "qemu-img",
            &[
                "convert",
                "-O",
                "raw",
                &image_path,
                &disk.display().to_string(),
            ],
        )?;
        Ok(())
    } else {
        cmd::run_stdout_to_file("unxz", &["-T0", "-c", &image_path], disk)
    }
}

pub struct VmCreateOptions<'a> {
    pub disk: &'a str,
    pub os: &'a str,
    pub version: Option<&'a str>,
    pub cloud_init: bool,
    pub ssh_key: Option<&'a str>,
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

fn seed_dataset(name: &str, config: &Config) -> Result<String> {
    validate_name(name)?;
    Ok(format!("{}{SEED_BASE}/{name}", config.zfs_pool))
}

fn seed_path(name: &str, config: &Config) -> Result<PathBuf> {
    Ok(PathBuf::from(format!(
        "/dev/zvol/{}",
        seed_dataset(name, config)?
    )))
}

fn read_config(name: &str, config: &Config) -> Result<VmConfig> {
    let dataset = vm_dataset(name, config)?;
    let value = |name: &str| crate::metadata::get_required(&dataset, name);
    let optional = |name: &str| crate::metadata::get(&dataset, name);
    Ok(VmConfig {
        name: name.to_string(),
        disk: PathBuf::from(format!("/dev/zvol/{dataset}/disk.vol")),
        os: value("os")?,
        iso: optional("iso")?.map(PathBuf::from),
        vnc_port: optional("vnc_port")?.map(|v| v.parse()).transpose()?,
        vnc_bind: value("vnc_bind")?,
        vnc_width: value("vnc_width")?.parse()?,
        vnc_height: value("vnc_height")?.parse()?,
        tpm: crate::metadata::get_bool(&dataset, "tpm", false)?,
        cpus: value("cpus")?.parse()?,
        memory: value("memory")?,
        dependencies: optional("dependencies")?
            .unwrap_or_default()
            .split(',')
            .filter(|dependency| !dependency.is_empty())
            .map(str::to_string)
            .collect(),
        enabled: crate::metadata::get_bool(&dataset, "enabled", true)?,
        interface_macs: optional(&interface_mac_property(VIRTIO_NET_INTERFACE))?
            .map(|mac| HashMap::from([(VIRTIO_NET_INTERFACE.to_string(), mac)]))
            .unwrap_or_default(),
    })
}

fn vm_dataset(name: &str, config: &Config) -> Result<String> {
    Ok(format!(
        "{}{}",
        config.zfs_pool,
        vm_dir(name, config)?.display()
    ))
}

pub fn create(name: &str, options: &VmCreateOptions<'_>, config: &Config) -> Result<()> {
    validate_os(options.os)?;
    let image_version = if options.cloud_init {
        Some(cloud_image_version(options.os, options.version)?)
    } else {
        options.version.map(str::to_string)
    };
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
    let vm_dataset_path = format!("{}{}", config.zfs_pool, dir.display());
    cmd::run("zfs", &["create", "-p", &vm_dataset_path])?;
    cmd::run("zfs", &["create", "-V", options.disk, &disk_dataset])?;
    let dataset = vm_dataset(name, config)?;
    crate::metadata::set(&dataset, "os", options.os)?;
    if let Some(iso) = iso_path.as_ref() {
        crate::metadata::set(&dataset, "iso", &iso.display().to_string())?;
    }
    if let Some(port) = options.vnc_port {
        crate::metadata::set(&dataset, "vnc_port", &port.to_string())?;
    }
    crate::metadata::set(&dataset, "vnc_bind", options.vnc_bind.trim())?;
    crate::metadata::set(&dataset, "vnc_width", &options.vnc_width.to_string())?;
    crate::metadata::set(&dataset, "vnc_height", &options.vnc_height.to_string())?;
    crate::metadata::set(&dataset, "tpm", &options.tpm.to_string())?;
    crate::metadata::set(&dataset, "cpus", &options.cpus.to_string())?;
    crate::metadata::set(&dataset, "memory", options.memory.trim())?;
    crate::metadata::set(&dataset, "dependencies", "")?;
    crate::metadata::set(&dataset, "enabled", "true")?;
    crate::metadata::set(
        &dataset,
        &interface_mac_property(VIRTIO_NET_INTERFACE),
        &random_mac_address()?,
    )?;
    if let Some(version) = image_version.as_deref() {
        if options.cloud_init {
            provision_cloud_image(options.os, version, &disk_path)?;
        }
    }
    if options.cloud_init {
        create_seed(name, options.os, options.ssh_key, config)?;
    }
    Ok(())
}

fn create_seed(name: &str, os: &str, ssh_key: Option<&str>, config: &Config) -> Result<()> {
    if let Some(ssh_key) = ssh_key {
        if ssh_key.trim().is_empty() || ssh_key.contains(['\n', '\r']) {
            bail!("SSH public key must be a single non-empty line");
        }
    }
    let dataset = seed_dataset(name, config)?;
    let device = seed_path(name, config)?;
    let mountpoint = vm_dir(name, config)?.join("seed");
    cmd::run("zfs", &["create", "-V", "256M", &dataset])?;
    cmd::run(
        "newfs_msdos",
        &[
            "-F",
            "32",
            "-c",
            "4",
            "-L",
            "cidata",
            &device.display().to_string(),
        ],
    )?;
    fs::create_dir_all(&mountpoint)?;
    cmd::run(
        "mount",
        &[
            "-t",
            "msdosfs",
            &device.display().to_string(),
            &mountpoint.display().to_string(),
        ],
    )?;
    let copy_result = (|| -> Result<()> {
        fs::write(
            mountpoint.join("meta-data"),
            format!("instance-id: {name}\nlocal-hostname: {name}\n"),
        )?;
        let root_password = crate::util::random_password()?;
        let ssh_authorized_keys = ssh_key
            .map(|key| {
                format!(
                    "    ssh_authorized_keys:\n      - '{}'\n",
                    key.replace('\'', "''")
                )
            })
            .unwrap_or_default();
        if os == "ubuntu" {
            fs::write(
                mountpoint.join("user-data"),
                format!(
                    r#"#cloud-config
package_update: true
package_upgrade: true
users:
  - default
  - name: provision
    plain_text_passwd: "provision"
    lock_passwd: false
    groups: [adm, sudo]
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    shell: /bin/bash
{ssh_authorized_keys}ssh_pwauth: true
chpasswd:
  list: |
    root:{root_password}
  expire: false
write_files:
  - path: /etc/default/grub.d/99-priosun-serial.cfg
    content: |
      GRUB_TERMINAL="serial"
      GRUB_SERIAL_COMMAND="serial --speed=115200 --unit=0 --word=8 --parity=no --stop=1"
      GRUB_CMDLINE_LINUX_DEFAULT="$GRUB_CMDLINE_LINUX_DEFAULT console=ttyS0,115200"
runcmd:
  - update-grub
  - systemctl enable --now serial-getty@ttyS0.service
power_state:
  mode: reboot
  message: Rebooting after package updates
  timeout: 30
"#
                ),
            )?;
        } else if os == "debian" {
            fs::write(
                mountpoint.join("user-data"),
                format!(
                    r#"#cloud-config
package_update: true
package_upgrade: true
users:
  - default
  - name: provision
    plain_text_passwd: provision
    lock_passwd: false
    groups: [sudo]
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    shell: /bin/bash
{ssh_authorized_keys}ssh_pwauth: true
chpasswd:
  list: |
    root:{root_password}
  expire: false
write_files:
  - path: /etc/default/grub.d/99-priosun-serial.cfg
    content: |
      GRUB_TERMINAL="serial"
      GRUB_SERIAL_COMMAND="serial --speed=115200 --unit=0 --word=8 --parity=no --stop=1"
      GRUB_CMDLINE_LINUX_DEFAULT="$GRUB_CMDLINE_LINUX_DEFAULT console=ttyS0,115200"
runcmd:
  - update-grub
  - systemctl enable --now serial-getty@ttyS0.service
power_state:
  mode: reboot
  message: Rebooting after package updates
  timeout: 30
"#
                ),
            )?;
        } else if os == "fedora" {
            fs::write(
                mountpoint.join("user-data"),
                format!(
                    r#"#cloud-config
package_update: true
package_upgrade: true
users:
  - default
  - name: provision
    plain_text_passwd: provision
    lock_passwd: false
    groups: [wheel]
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    shell: /bin/bash
{ssh_authorized_keys}ssh_pwauth: true
chpasswd:
  list: |
    root:{root_password}
  expire: false
runcmd:
  - grubby --update-kernel=ALL --args=console=ttyS0,115200
  - systemctl enable --now serial-getty@ttyS0.service
power_state:
  mode: reboot
  message: Rebooting after package updates
  timeout: 30
"#
                ),
            )?;
        } else if os == "freebsd" {
            fs::write(
                mountpoint.join("user-data"),
                format!(
                    r#"#cloud-config
users:
  - default
  - name: provision
    plain_text_passwd: "provision"
    lock_passwd: false
    groups: "wheel"
    shell: "/bin/sh"
{ssh_authorized_keys}ssh_pwauth: true
chpasswd:
  expire: false
  users:
    - name: root
      password: "RANDOM"
write_files:
  - path: /etc/rc.conf.d/kld
    append: true
    content: |
      kld_list="mac_do"
  - path: /etc/sysctl.conf
    append: true
    content: |
      security.mac.do.rules=gid=0:any
"#
                ),
            )?;
        }
        validate_yaml(&mountpoint.join("meta-data"))?;
        validate_yaml(&mountpoint.join("user-data"))?;
        Ok(())
    })();
    let unmount_result = cmd::run("umount", &[&mountpoint.display().to_string()]);
    copy_result?;
    unmount_result?;
    fs::remove_dir(&mountpoint)?;
    Ok(())
}

fn validate_yaml(path: &Path) -> Result<()> {
    let script = r#"
local yaml = require("lyaml")
local content = io.read("*a")
assert(yaml.load(content))
"#;
    let content = fs::read(path)?;
    cmd::run_with_stdin("/usr/libexec/flua", &["-e", script], &content)
        .with_context(|| format!("invalid cloud-init YAML: {}", path.display()))?;
    Ok(())
}

pub fn set_dependencies(name: &str, dependencies: Vec<String>, config: &Config) -> Result<()> {
    crate::dependency::validate(name, &dependencies, config)?;
    let dataset = vm_dataset(name, config)?;
    crate::metadata::set(&dataset, "dependencies", &dependencies.join(","))?;
    Ok(())
}

pub fn set_enabled(name: &str, enabled: bool, config: &Config) -> Result<()> {
    let dataset = vm_dataset(name, config)?;
    crate::metadata::set(&dataset, "enabled", &enabled.to_string())
}

pub fn path_exists(name: &str, config: &Config) -> bool {
    vm_dir(name, config)
        .map(|path| path.is_dir())
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

type SerialConsole = Arc<Mutex<fs::File>>;

static SERIAL_CONSOLES: OnceLock<Mutex<HashMap<String, SerialConsole>>> = OnceLock::new();

fn serial_consoles() -> &'static Mutex<HashMap<String, SerialConsole>> {
    SERIAL_CONSOLES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn create_serial_console() -> Result<(fs::File, fs::File)> {
    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        bail!("posix_openpt: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::grantpt(master_fd) } < 0 || unsafe { libc::unlockpt(master_fd) } < 0 {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(master_fd) };
        bail!("prepare VM serial PTY: {error}");
    }
    let slave_name = unsafe { libc::ptsname(master_fd) };
    if slave_name.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(master_fd) };
        bail!("get VM serial PTY name: {error}");
    }
    let slave_fd = unsafe { libc::open(slave_name, libc::O_RDWR | libc::O_NOCTTY) };
    if slave_fd < 0 {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(master_fd) };
        bail!("open VM serial PTY: {error}");
    }
    let mut terminal = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(slave_fd, &mut terminal) } < 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        bail!("read VM serial PTY attributes: {error}");
    }
    unsafe { libc::cfmakeraw(&mut terminal) };
    if unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &terminal) } < 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        bail!("set VM serial PTY raw mode: {error}");
    }
    Ok(unsafe {
        (
            fs::File::from_raw_fd(master_fd),
            fs::File::from_raw_fd(slave_fd),
        )
    })
}

fn register_serial_console(name: &str, console: SerialConsole) -> Result<()> {
    let mut consoles = serial_consoles()
        .lock()
        .map_err(|_| anyhow::anyhow!("VM serial console registry is poisoned"))?;
    consoles.insert(name.to_string(), console);
    Ok(())
}

fn remove_serial_console(name: &str) {
    if let Ok(mut consoles) = serial_consoles().lock() {
        consoles.remove(name);
    }
}

pub fn serial_console(name: &str, config: &Config) -> Result<fs::File> {
    validate_name(name)?;
    if !is_running(name, config) {
        bail!("VM is not running: {name}");
    }
    let consoles = serial_consoles()
        .lock()
        .map_err(|_| anyhow::anyhow!("VM serial console registry is poisoned"))?;
    let console = consoles
        .get(name)
        .cloned()
        .with_context(|| format!("serial console is unavailable for VM {name}"))?;
    drop(consoles);
    let result = console
        .lock()
        .map_err(|_| anyhow::anyhow!("VM serial console is poisoned"))?
        .try_clone()
        .context("failed to clone VM serial console");
    result
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

pub fn start_with_seed(name: &str, config: &Config) -> Result<()> {
    start_with_stack_mode(name, config, &mut HashSet::new(), true)
}

pub(crate) fn start_with_stack(
    name: &str,
    config: &Config,
    stack: &mut HashSet<String>,
) -> Result<()> {
    start_with_stack_mode(name, config, stack, false)
}

fn start_with_stack_mode(
    name: &str,
    config: &Config,
    stack: &mut HashSet<String>,
    use_seed: bool,
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
    let result = start_inner(name, config, use_seed);
    stack.remove(name);
    result
}

fn start_inner(name: &str, config: &Config, use_seed: bool) -> Result<()> {
    let vm = read_config(name, config)?;
    validate_os(&vm.os)?;
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
    let mac = if let Some(mac) = vm.interface_macs.get(VIRTIO_NET_INTERFACE) {
        mac.clone()
    } else {
        let mac = random_mac_address()?;
        crate::metadata::set(
            &vm_dataset(name, config)?,
            &interface_mac_property(VIRTIO_NET_INTERFACE),
            &mac,
        )?;
        mac
    };
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
        format!("2,virtio-net,{},mac={mac}", tap),
        "-s".to_string(),
        "31,lpc".to_string(),
    ];
    args.extend(["-l".to_string(), "com1,stdio".to_string()]);
    if use_seed && vm.iso.is_some() {
        bail!("VM cannot use both cloud-init and an installation ISO");
    }
    if use_seed && zfs_dataset_exists(&seed_dataset(name, config)?) {
        args.extend([
            "-s".to_string(),
            format!("4,nvme,{}", seed_path(name, config)?.display()),
        ]);
    } else if let Some(iso) = vm.iso {
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
    cmd::announce_command("bhyve", &args);
    let (serial_master, serial_slave) = create_serial_console()?;
    let serial_stdin = serial_slave.try_clone()?;
    let serial_stdout = serial_slave.try_clone()?;
    register_serial_console(name, Arc::new(Mutex::new(serial_master)))?;
    let child = match Command::new("bhyve")
        .args(&args)
        .stdin(Stdio::from(serial_stdin))
        .stdout(Stdio::from(serial_stdout))
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
            remove_serial_console(name);
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
    let vm_name = vm.name.clone();
    let vm_config = config.clone();
    let cleanup_seed = use_seed;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
        remove_serial_console(&vm_name);
        destroy_bhyve_vm(&vm_name);
        if let Some(mut tpm) = tpm_child {
            let _ = tpm.kill();
            let _ = tpm.wait();
        }
        destroy_tap(&tap, &bridge);
        let _ = fs::remove_file(bhyve_pid);
        let _ = fs::remove_file(tpm_pid);
        let _ = fs::remove_file(tap_file);
        if cleanup_seed {
            if let Ok(seed) = seed_dataset(&vm_name, &vm_config) {
                let _ = Command::new("zfs")
                    .args(["destroy", "-f", &seed])
                    .stdout(Stdio::null())
                    .status();
            }
        }
    });
    Ok(())
}

pub fn stop(name: &str, config: &Config) -> Result<()> {
    let _ = read_config(name, config)?;
    if let Ok(pid_text) = fs::read_to_string(pid_path(name, config)?) {
        if let Ok(pid) = pid_text.trim().parse::<libc::pid_t>() {
            let kill_result = unsafe { libc::kill(pid, libc::SIGTERM) };
            if kill_result == -1
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("failed to stop VM {name}"));
            }
            wait_for_exit(pid, name)?;
            wait_for_cleanup(&pid_path(name, config)?, name)?;
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

fn wait_for_exit(pid: libc::pid_t, name: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = unsafe { libc::kill(pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("VM {name} did not stop within 30 seconds");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_cleanup(path: &Path, name: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while path.exists() {
        if Instant::now() >= deadline {
            bail!("cleanup of VM {name} did not complete within 30 seconds");
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn destroy_bhyve_vm(name: &str) {
    let vm_arg = format!("--vm={name}");
    let _ = cmd::run("bhyvectl", &["--destroy", &vm_arg]);
}

fn zfs_dataset_exists(dataset: &str) -> bool {
    Command::new("zfs")
        .args(["list", "-H", "-o", "name", dataset])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn destroy(name: &str, config: &Config) -> Result<()> {
    let dir = vm_dir(name, config)?;
    if !dir.exists() {
        bail!("VM does not exist: {}", name);
    }
    let dataset = vm_dataset(name, config)?;
    let seed = seed_dataset(name, config)?;
    stop(name, config)?;
    crate::dependency::remove_references(name, config)?;
    cmd::run("zfs", &["destroy", "-r", "-f", &dataset])?;
    if zfs_dataset_exists(&seed) {
        cmd::run("zfs", &["destroy", "-f", &seed])?;
    }
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
            if path_exists(&name, config) {
                result.push((name.clone(), is_running(&name, config)));
            }
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}
