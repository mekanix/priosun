use anyhow::{anyhow, bail, Context, Result};
use nvtree::{nvtree_add, nvtree_find, nvtree_pack, nvtree_unpack, Nvtree, Nvtvalue};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

macro_rules! socket_base {
    () => {
        "/var/run/priosun"
    };
}

pub const SOCKET_BASE: &str = socket_base!();
pub const SOCKET_PATH: &str = concat!(socket_base!(), "/socket");

const SUPPORTED_COMMANDS: &[&str] = &[
    "create",
    "destroy",
    "start",
    "stop",
    "attach",
    "version",
    "network-init",
    "up",
    "down",
    "list",
    "dependencies",
    "enable",
    "disable",
];

#[derive(Debug, Clone, Copy)]
pub enum Resource {
    Dataset,
    Volume,
    Vm,
    Jail,
    All,
}

fn write_message(stream: &mut UnixStream, data: &[u8]) -> Result<()> {
    let length = u32::try_from(data.len()).context("message is too large")?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(data)?;
    Ok(())
}

fn read_message(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > 16 * 1024 * 1024 {
        bail!("message is too large");
    }
    let mut data = vec![0u8; length];
    stream.read_exact(&mut data)?;
    Ok(data)
}

pub fn receive_message(stream: &mut UnixStream) -> Result<Nvtree> {
    nvtree_unpack(&read_message(stream)?)
        .map_err(|error| anyhow!("invalid nvtree message: {:?}", error))
}

pub fn send_message(stream: &mut UnixStream, message: &Nvtree) -> Result<()> {
    write_message(stream, &nvtree_pack(message))
}

pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            socket: path.as_ref().to_path_buf(),
        }
    }

    pub fn list(&self, resource: Resource) -> Result<Nvtree> {
        let resource = match resource {
            Resource::Dataset => "datasets",
            Resource::Volume => "volumes",
            Resource::Vm => "vms",
            Resource::Jail => "jails",
            Resource::All => "all",
        };
        let mut request = nvtree::nvtree_create(0);
        add_string(&mut request, "command", "list");
        add_string(&mut request, "resource", resource);
        send_request_tree(&self.socket, &request)
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new(SOCKET_PATH)
    }
}

pub fn send_request(args: &[String]) -> Result<Nvtree> {
    if args.is_empty() {
        bail!("request is missing command");
    }
    let request = request_from_args(args)?;
    send_request_tree(Path::new(SOCKET_PATH), &request)
}

fn send_request_tree(socket: &Path, request: &Nvtree) -> Result<Nvtree> {
    let command = match nvtree_find(request, "command").map(|pair| &pair.value) {
        Some(Nvtvalue::String(command)) => command.as_str(),
        _ => "unknown",
    };
    if !SUPPORTED_COMMANDS.contains(&command) {
        bail!("unsupported command: {command}");
    }
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("failed to connect to {}", socket.display()))?;
    send_message(&mut stream, request)?;
    let interactive = command == "attach"
        || (command == "start" && optional_bool(request, "attach") == Some(true));
    let _terminal = if interactive {
        TerminalMode::new(std::io::stdin().as_raw_fd())?
    } else {
        None
    };
    let detached = Arc::new(AtomicBool::new(false));
    let detach_enabled = Arc::new(AtomicBool::new(false));
    if interactive {
        let mut input_stream = stream.try_clone()?;
        let input_detached = Arc::clone(&detached);
        let input_detach_enabled = Arc::clone(&detach_enabled);
        std::thread::spawn(move || {
            let mut input = std::io::stdin();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = match input.read(&mut buffer) {
                    Ok(0) | Err(_) => {
                        let mut message = nvtree::nvtree_create(0);
                        add_string(&mut message, "event", "stdin_eof");
                        let _ = send_message(&mut input_stream, &message);
                        break;
                    }
                    Ok(count) => count,
                };
                let detach_at = if input_detach_enabled.load(Ordering::Acquire) {
                    buffer[..count].iter().position(|byte| *byte == 0x1d)
                } else {
                    None
                };
                let data_end = detach_at.unwrap_or(count);
                if data_end > 0 {
                    let mut message = nvtree::nvtree_create(0);
                    add_string(&mut message, "event", "stdin");
                    add_string(
                        &mut message,
                        "data",
                        &String::from_utf8_lossy(&buffer[..data_end]),
                    );
                    if send_message(&mut input_stream, &message).is_err() {
                        break;
                    }
                }
                if detach_at.is_some() {
                    input_detached.store(true, Ordering::Release);
                    let _ = input_stream.shutdown(Shutdown::Both);
                    break;
                }
            }
        });
    }
    loop {
        let response = match receive_message(&mut stream) {
            Ok(response) => response,
            Err(_error) if detached.load(Ordering::Acquire) => {
                return Ok(success(0, &[], &[]));
            }
            Err(error) => return Err(error),
        };
        if let Some(event) = nvtree_find(&response, "event").map(|pair| &pair.value) {
            if event_allows_detach(event) {
                detach_enabled.store(true, Ordering::Release);
                eprintln!("press Ctrl-] to detach");
            }
            print_event(event)?;
        } else {
            return Ok(response);
        }
    }
}

struct TerminalMode {
    fd: i32,
    original: libc::termios,
}

impl TerminalMode {
    fn new(fd: i32) -> Result<Option<Self>> {
        if unsafe { libc::isatty(fd) } == 0 {
            return Ok(None);
        }
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } < 0 {
            bail!(
                "read terminal attributes: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } < 0 {
            bail!("set raw terminal mode: {}", std::io::Error::last_os_error());
        }
        Ok(Some(Self { fd, original }))
    }
}

impl Drop for TerminalMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

fn print_event(event: &Nvtvalue) -> Result<()> {
    let Nvtvalue::Nested(event) = event else {
        bail!("invalid event from priosund");
    };
    if let Some(Nvtvalue::String(command)) = nvtree_find(event, "command").map(|pair| &pair.value) {
        println!("$ {command}");
    }
    if let Some(Nvtvalue::String(stdout)) = nvtree_find(event, "stdout").map(|pair| &pair.value) {
        let mut output = std::io::stdout();
        output.write_all(stdout.as_bytes())?;
        output.flush()?;
    }
    if let Some(Nvtvalue::String(stderr)) = nvtree_find(event, "stderr").map(|pair| &pair.value) {
        let mut output = std::io::stderr();
        output.write_all(stderr.as_bytes())?;
        output.flush()?;
    }
    if let Some(Nvtvalue::String(message)) = nvtree_find(event, "message").map(|pair| &pair.value) {
        println!("{message}");
    }
    Ok(())
}

pub fn receive_request(stream: &mut UnixStream) -> Result<Nvtree> {
    let request = receive_message(stream)?;
    let command = match nvtree_find(&request, "command").map(|pair| &pair.value) {
        Some(Nvtvalue::String(command)) => command.clone(),
        _ => bail!("request is missing command"),
    };
    if !SUPPORTED_COMMANDS.contains(&command.as_str()) {
        bail!("unsupported command: {command}");
    }
    Ok(request)
}

pub fn request_to_args(request: &Nvtree) -> Result<Vec<String>> {
    let command = string_field(request, "command")?;
    let mut args = vec![command.to_string()];
    match command {
        "create" => {
            let resource = string_field(request, "type")?;
            args.push(resource.to_string());
            match resource {
                "dataset" => args.push(string_field(request, "dataset")?.to_string()),
                "volume" => {
                    args.push(string_field(request, "volume")?.to_string());
                    args.extend([
                        "--size".to_string(),
                        number_field(request, "size")?.to_string(),
                    ]);
                }
                "jail" => {
                    args.push(string_field(request, "name")?.to_string());
                    append_string_option(request, &mut args, "set", "--set")?;
                    append_string_option(request, &mut args, "base", "--base")?;
                    append_string_option(request, &mut args, "version", "--version")?;
                    append_string_option(request, &mut args, "ssh_key", "--ssh-key")?;
                    if bool_field(request, "cloud_init")? {
                        args.push("--start".to_string());
                    }
                    if optional_bool(request, "attach") == Some(true) {
                        args.push("--attach".to_string());
                    }
                }
                "base" => {
                    args.push(string_field(request, "resource")?.to_string());
                    args.push(string_field(request, "name")?.to_string());
                    append_string_option(request, &mut args, "set", "--set")?;
                    append_string_option(request, &mut args, "version", "--version")?;
                }
                "vm" => {
                    args.push(string_field(request, "name")?.to_string());
                    args.extend([
                        "--disk".to_string(),
                        string_field(request, "disk")?.to_string(),
                        "--os".to_string(),
                        string_field(request, "os")?.to_string(),
                    ]);
                    append_string_option(request, &mut args, "version", "--version")?;
                    if bool_field(request, "cloud_init")? {
                        args.push("--cloud-init".to_string());
                    }
                    append_string_option(request, &mut args, "ssh_key", "--ssh-key")?;
                    append_string_option(request, &mut args, "iso", "--iso")?;
                    append_number_option(request, &mut args, "vnc_port", "--vnc-port")?;
                    append_string_option(request, &mut args, "vnc_bind", "--vnc-bind")?;
                    append_number_option(request, &mut args, "vnc_width", "--vnc-width")?;
                    append_number_option(request, &mut args, "vnc_height", "--vnc-height")?;
                    if bool_field(request, "tpm")? {
                        args.push("--tpm".to_string());
                    }
                    append_number_option(request, &mut args, "cpus", "--cpus")?;
                    append_string_option(request, &mut args, "memory", "--memory")?;
                    if optional_bool(request, "start") == Some(true) {
                        args.push("--start".to_string());
                    }
                    if optional_bool(request, "attach") == Some(true) {
                        args.push("--attach".to_string());
                    }
                }
                _ => bail!("unsupported create type: {resource}"),
            }
        }
        "destroy" => {
            args.push(string_field(request, "type")?.to_string());
            if string_field(request, "type")? == "base" {
                args.push(string_field(request, "resource")?.to_string());
            }
            args.push(string_field(request, "name")?.to_string());
        }
        "start" | "stop" | "attach" => {
            args.push(string_field(request, "name")?.to_string());
            if command == "start" && optional_bool(request, "attach") == Some(true) {
                args.push("--attach".to_string());
            }
        }
        "dependencies" => {
            args.push(string_field(request, "type")?.to_string());
            args.push(string_field(request, "name")?.to_string());
            args.push(string_array_field(request, "dependencies")?.join(","));
        }
        "enable" | "disable" => {
            args.push(string_field(request, "name")?.to_string());
        }
        "list" => args.push(string_field(request, "resource")?.to_string()),
        "version" => {}
        "network-init" => {}
        "up" | "down" => {
            args.extend([
                "--container".to_string(),
                string_field(request, "container")?.to_string(),
                "--develop".to_string(),
                bool_field(request, "develop")?.to_string(),
                "--service-dir".to_string(),
                string_field(request, "service_dir")?.to_string(),
                string_field(request, "name")?.to_string(),
            ]);
        }
        _ => bail!("unsupported command: {command}"),
    }
    Ok(args)
}

fn string_field<'a>(tree: &'a Nvtree, name: &str) -> Result<&'a str> {
    match nvtree_find(tree, name).map(|pair| &pair.value) {
        Some(Nvtvalue::String(value)) => Ok(value),
        _ => bail!("request is missing string field {name}"),
    }
}

fn number_field(tree: &Nvtree, name: &str) -> Result<u64> {
    match nvtree_find(tree, name).map(|pair| &pair.value) {
        Some(Nvtvalue::Number(value)) => Ok(*value),
        _ => bail!("request is missing numeric field {name}"),
    }
}

fn bool_field(tree: &Nvtree, name: &str) -> Result<bool> {
    match nvtree_find(tree, name).map(|pair| &pair.value) {
        Some(Nvtvalue::Bool(value)) => Ok(*value),
        _ => bail!("request is missing boolean field {name}"),
    }
}

fn optional_bool(tree: &Nvtree, name: &str) -> Option<bool> {
    nvtree_find(tree, name).and_then(|pair| match &pair.value {
        Nvtvalue::Bool(value) => Some(*value),
        _ => None,
    })
}

fn string_array_field<'a>(tree: &'a Nvtree, name: &str) -> Result<&'a [String]> {
    match nvtree_find(tree, name).map(|pair| &pair.value) {
        Some(Nvtvalue::StringArray(value)) => Ok(value),
        _ => bail!("request is missing string array field {name}"),
    }
}

fn append_string_option(
    tree: &Nvtree,
    args: &mut Vec<String>,
    field: &str,
    option: &str,
) -> Result<()> {
    if let Some(Nvtvalue::String(value)) = nvtree_find(tree, field).map(|pair| &pair.value) {
        args.extend([option.to_string(), value.clone()]);
    }
    Ok(())
}

fn append_number_option(
    tree: &Nvtree,
    args: &mut Vec<String>,
    field: &str,
    option: &str,
) -> Result<()> {
    if let Some(Nvtvalue::Number(value)) = nvtree_find(tree, field).map(|pair| &pair.value) {
        args.extend([option.to_string(), value.to_string()]);
    }
    Ok(())
}

pub fn request_from_args(args: &[String]) -> Result<Nvtree> {
    let Some(command) = args.first() else {
        bail!("request is missing command");
    };
    if !SUPPORTED_COMMANDS.contains(&command.as_str()) {
        bail!("unsupported command: {command}");
    }

    let mut request = nvtree::nvtree_create(0);
    add_string(&mut request, "command", command);
    match command.as_str() {
        "create" => encode_create(&mut request, &args[1..])?,
        "destroy" => {
            let resource_type = require_arg(&args[1..], 0, "type")?;
            add_string(&mut request, "type", resource_type);
            if resource_type == "base" {
                add_string(
                    &mut request,
                    "resource",
                    require_arg(&args[1..], 1, "resource")?,
                );
                add_string(&mut request, "name", require_arg(&args[1..], 2, "name")?);
            } else {
                add_string(&mut request, "name", require_arg(&args[1..], 1, "name")?);
            }
        }
        "start" | "stop" | "attach" => {
            require_arg(&args[1..], 0, "name")
                .map(|value| add_string(&mut request, "name", value))?;
            if command == "start" {
                add_bool(&mut request, "attach", has_flag(&args[1..], "--attach"));
                ensure_no_extra(
                    &args[1..]
                        .iter()
                        .filter(|arg| arg.as_str() != "--attach")
                        .cloned()
                        .collect::<Vec<_>>(),
                    1,
                )?;
            }
        }
        "dependencies" => {
            require_arg(&args[1..], 0, "type")
                .map(|value| add_string(&mut request, "type", value))?;
            require_arg(&args[1..], 1, "name")
                .map(|value| add_string(&mut request, "name", value))?;
            let dependencies = require_arg(&args[1..], 2, "dependencies")?
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect();
            nvtree_add(
                &mut request,
                nvtree::Nvtpair {
                    flags: 0,
                    name: "dependencies".to_string(),
                    value: Nvtvalue::StringArray(dependencies),
                },
            );
        }
        "enable" | "disable" => {
            require_arg(&args[1..], 0, "name")
                .map(|value| add_string(&mut request, "name", value))?;
            ensure_no_extra(args, 2)?;
        }
        "list" => {
            require_arg(&args[1..], 0, "resource")
                .map(|value| add_string(&mut request, "resource", value))?;
        }
        "version" => {
            if args.len() != 1 {
                bail!("version does not accept arguments");
            }
        }
        "network-init" => {
            if args.len() != 1 {
                bail!("network-init does not accept arguments");
            }
        }
        "up" | "down" => {
            let container = optional_option(&args[1..], "--container")?.unwrap_or("jail");
            if container != "jail" {
                bail!("unsupported container: {container}");
            }
            let develop = optional_option(&args[1..], "--develop")?.unwrap_or("false");
            let develop = develop
                .parse::<bool>()
                .with_context(|| format!("invalid --develop value: {develop}"))?;
            let service_dir = optional_option(&args[1..], "--service-dir")?
                .ok_or_else(|| anyhow!("{command} requires --service-dir"))?;
            let mut name = None;
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--container" | "--develop" | "--service-dir" => index += 2,
                    value if value.starts_with('-') => {
                        bail!("unsupported {command} option: {value}");
                    }
                    value => {
                        if name.replace(value).is_some() {
                            bail!("unexpected {command} arguments");
                        }
                        index += 1;
                    }
                }
            }
            let name = name.ok_or_else(|| anyhow!("{command} requires a service name"))?;
            if name.starts_with('-') {
                bail!("{command} requires a service name");
            }
            add_string(&mut request, "container", container);
            add_bool(&mut request, "develop", develop);
            add_string(&mut request, "service_dir", service_dir);
            add_string(&mut request, "name", name);
        }
        _ => unreachable!(),
    }
    Ok(request)
}

fn encode_create(request: &mut Nvtree, args: &[String]) -> Result<()> {
    let resource = require_arg(args, 0, "type")?;
    add_string(request, "type", resource);
    match resource {
        "dataset" => {
            add_string(request, "dataset", require_arg(args, 1, "dataset")?);
            ensure_no_extra(args, 2)?;
        }
        "volume" => {
            add_string(request, "volume", require_arg(args, 1, "volume")?);
            let size = named_option(args, "--size")?;
            add_number(request, "size", parse_size(size)?);
        }
        "jail" => {
            add_string(request, "name", require_arg(args, 1, "name")?);
            encode_optional_string(request, args, "--set", "set")?;
            encode_optional_string(request, args, "--base", "base")?;
            encode_optional_string(request, args, "--version", "version")?;
            encode_optional_string(request, args, "--ssh-key", "ssh_key")?;
            add_bool(request, "start", has_flag(args, "--start"));
            if has_flag(args, "--attach") && !has_flag(args, "--start") {
                bail!("--attach requires --start");
            }
            add_bool(request, "attach", has_flag(args, "--attach"));
        }
        "base" => {
            add_string(request, "resource", require_arg(args, 1, "resource")?);
            add_string(request, "name", require_arg(args, 2, "name")?);
            match args[1].as_str() {
                "jail" => {
                    encode_optional_string(request, args, "--set", "set")?;
                    encode_optional_string(request, args, "--version", "version")?;
                }
                "vm" => ensure_no_extra(args, 3)?,
                resource => bail!("unsupported base resource: {resource}"),
            }
        }
        "vm" => {
            if args.iter().any(|arg| arg == "--cloud-init") && args.iter().any(|arg| arg == "--iso")
            {
                bail!("--cloud-init and --iso are mutually exclusive");
            }
            add_string(request, "name", require_arg(args, 1, "name")?);
            add_string(request, "disk", named_option(args, "--disk")?);
            add_string(request, "os", named_option(args, "--os")?);
            encode_optional_string(request, args, "--version", "version")?;
            add_bool(request, "cloud_init", has_flag(args, "--cloud-init"));
            if args.iter().any(|arg| arg == "--ssh-key")
                && !args.iter().any(|arg| arg == "--cloud-init")
            {
                bail!("--ssh-key requires --cloud-init");
            }
            encode_optional_string(request, args, "--ssh-key", "ssh_key")?;
            encode_optional_string(request, args, "--iso", "iso")?;
            encode_optional_number(request, args, "--vnc-port", "vnc_port")?;
            encode_optional_string(request, args, "--vnc-bind", "vnc_bind")?;
            encode_optional_number(request, args, "--vnc-width", "vnc_width")?;
            encode_optional_number(request, args, "--vnc-height", "vnc_height")?;
            add_bool(request, "tpm", has_flag(args, "--tpm"));
            encode_optional_number(request, args, "--cpus", "cpus")?;
            encode_optional_string(request, args, "--memory", "memory")?;
            add_bool(request, "start", has_flag(args, "--cloud-init"));
            if has_flag(args, "--attach") && !has_flag(args, "--cloud-init") {
                bail!("--attach requires --cloud-init");
            }
            add_bool(request, "attach", has_flag(args, "--attach"));
        }
        _ => bail!("unsupported create type: {resource}"),
    }
    Ok(())
}

fn require_arg<'a>(args: &'a [String], index: usize, name: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing {name}"))
}

fn named_option<'a>(args: &'a [String], option: &str) -> Result<&'a str> {
    let position = args
        .iter()
        .position(|arg| arg == option)
        .ok_or_else(|| anyhow!("missing {option}"))?;
    args.get(position + 1)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing value for {option}"))
}

fn encode_optional_string(
    request: &mut Nvtree,
    args: &[String],
    option: &str,
    name: &str,
) -> Result<()> {
    if let Some(value) = optional_option(args, option)? {
        add_string(request, name, value);
    }
    Ok(())
}

fn encode_optional_number(
    request: &mut Nvtree,
    args: &[String],
    option: &str,
    name: &str,
) -> Result<()> {
    if let Some(value) = optional_option(args, option)? {
        add_number(
            request,
            name,
            value.parse().with_context(|| format!("invalid {option}"))?,
        );
    }
    Ok(())
}

fn optional_option<'a>(args: &'a [String], option: &str) -> Result<Option<&'a str>> {
    let Some(position) = args.iter().position(|arg| arg == option) else {
        return Ok(None);
    };
    Ok(Some(
        args.get(position + 1)
            .map(String::as_str)
            .ok_or_else(|| anyhow!("missing value for {option}"))?,
    ))
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn ensure_no_extra(args: &[String], from: usize) -> Result<()> {
    if args.len() > from {
        bail!("unexpected arguments: {:?}", &args[from..]);
    }
    Ok(())
}

fn parse_size(value: &str) -> Result<u64> {
    let (number, multiplier) = match value.chars().last() {
        Some('K' | 'k') => (&value[..value.len() - 1], 1_000_u64),
        Some('M' | 'm') => (&value[..value.len() - 1], 1_000_000_u64),
        Some('G' | 'g') => (&value[..value.len() - 1], 1_000_000_000_u64),
        Some('T' | 't') => (&value[..value.len() - 1], 1_000_000_000_000_u64),
        _ => (value, 1),
    };
    Ok(number.parse::<u64>()?.saturating_mul(multiplier))
}

fn add_string(tree: &mut Nvtree, name: &str, value: &str) {
    nvtree_add(tree, nvtree::nvtree_string(name, value));
}

fn add_number(tree: &mut Nvtree, name: &str, value: u64) {
    nvtree_add(tree, nvtree::nvtree_number(name, value));
}

fn add_bool(tree: &mut Nvtree, name: &str, value: bool) {
    nvtree_add(tree, nvtree::nvtree_bool(name, value));
}

pub fn send_response(stream: &mut UnixStream, response: &Nvtree) -> Result<()> {
    send_message(stream, response)
}

pub fn command_event(command: &str, stdout: Option<&[u8]>, stderr: Option<&[u8]>) -> Nvtree {
    command_event_internal(command, stdout, stderr, false)
}

pub fn serial_console_event() -> Nvtree {
    command_event_internal("serial console", None, None, true)
}

fn command_event_internal(
    command: &str,
    stdout: Option<&[u8]>,
    stderr: Option<&[u8]>,
    allows_detach: bool,
) -> Nvtree {
    let mut event = nvtree::nvtree_create(0);
    if !command.is_empty() {
        nvtree_add(&mut event, nvtree::nvtree_string("command", command));
    }
    if let Some(stdout) = stdout {
        nvtree_add(
            &mut event,
            nvtree::nvtree_string("stdout", &String::from_utf8_lossy(stdout)),
        );
    }
    if let Some(stderr) = stderr {
        nvtree_add(
            &mut event,
            nvtree::nvtree_string("stderr", &String::from_utf8_lossy(stderr)),
        );
    }
    if allows_detach {
        nvtree_add(&mut event, nvtree::nvtree_bool("detach", true));
    }
    let mut envelope = nvtree::nvtree_create(0);
    nvtree_add(
        &mut envelope,
        nvtree::Nvtpair {
            flags: 0,
            name: "event".to_string(),
            value: Nvtvalue::Nested(Box::new(event)),
        },
    );
    envelope
}

fn event_allows_detach(event: &Nvtvalue) -> bool {
    let Nvtvalue::Nested(event) = event else {
        return false;
    };
    matches!(
        nvtree_find(event, "detach").map(|pair| &pair.value),
        Some(Nvtvalue::Bool(true))
    )
}

pub fn message_event(message: &str) -> Nvtree {
    let mut event = nvtree::nvtree_create(0);
    nvtree_add(&mut event, nvtree::nvtree_string("message", message));
    let mut envelope = nvtree::nvtree_create(0);
    nvtree_add(
        &mut envelope,
        nvtree::Nvtpair {
            flags: 0,
            name: "event".to_string(),
            value: Nvtvalue::Nested(Box::new(event)),
        },
    );
    envelope
}

pub fn success(status: i32, stdout: &[u8], stderr: &[u8]) -> Nvtree {
    let mut payload = nvtree::nvtree_create(0);
    nvtree_add(
        &mut payload,
        nvtree::nvtree_number("status", status.max(0) as u64),
    );
    nvtree_add(
        &mut payload,
        nvtree::nvtree_string("stdout", &String::from_utf8_lossy(stdout)),
    );
    nvtree_add(
        &mut payload,
        nvtree::nvtree_string("stderr", &String::from_utf8_lossy(stderr)),
    );
    let mut envelope = nvtree::nvtree_create(0);
    nvtree_add(
        &mut envelope,
        nvtree::Nvtpair {
            flags: 0,
            name: "response".to_string(),
            value: Nvtvalue::Nested(Box::new(payload)),
        },
    );
    envelope
}

pub fn jails_success(jails: Vec<Nvtree>) -> Nvtree {
    structured_success("jails", jails)
}

pub fn datasets_success(datasets: Vec<Nvtree>) -> Nvtree {
    structured_success("datasets", datasets)
}

pub fn volumes_success(volumes: Vec<Nvtree>) -> Nvtree {
    structured_success("volumes", volumes)
}

pub fn vms_success(vms: Vec<Nvtree>) -> Nvtree {
    structured_success("vms", vms)
}

fn structured_success(resource: &str, rows: Vec<Nvtree>) -> Nvtree {
    let mut payload = nvtree::nvtree_create(0);
    nvtree_add(
        &mut payload,
        nvtree::Nvtpair {
            flags: 0,
            name: resource.to_string(),
            value: Nvtvalue::NestedArray(rows),
        },
    );

    let mut envelope = nvtree::nvtree_create(0);
    nvtree_add(
        &mut envelope,
        nvtree::Nvtpair {
            flags: 0,
            name: "response".to_string(),
            value: Nvtvalue::Nested(Box::new(payload)),
        },
    );
    envelope
}

pub fn error(message: &str) -> Nvtree {
    let mut envelope = nvtree::nvtree_create(0);
    nvtree_add(&mut envelope, nvtree::nvtree_string("error", message));
    envelope
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_dataset_uses_named_field() {
        let request = request_from_args(&[
            "create".to_string(),
            "dataset".to_string(),
            "zroot/data".to_string(),
        ])
        .unwrap();

        assert!(matches!(
            nvtree_find(&request, "command").unwrap().value,
            Nvtvalue::String(ref value) if value == "create"
        ));
        assert!(matches!(
            nvtree_find(&request, "type").unwrap().value,
            Nvtvalue::String(ref value) if value == "dataset"
        ));
        assert!(matches!(
            nvtree_find(&request, "dataset").unwrap().value,
            Nvtvalue::String(ref value) if value == "zroot/data"
        ));
        assert!(nvtree_find(&request, "arguments").is_none());
    }

    #[test]
    fn create_volume_uses_numeric_size() {
        let request = request_from_args(&[
            "create".to_string(),
            "volume".to_string(),
            "zroot/windows".to_string(),
            "--size".to_string(),
            "32G".to_string(),
        ])
        .unwrap();

        assert!(matches!(
            nvtree_find(&request, "volume").unwrap().value,
            Nvtvalue::String(ref value) if value == "zroot/windows"
        ));
        assert!(matches!(
            nvtree_find(&request, "size").unwrap().value,
            Nvtvalue::Number(value) if value == 32_000_000_000
        ));
        assert!(nvtree_find(&request, "arguments").is_none());
    }
}
