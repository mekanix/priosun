use anyhow::{anyhow, bail, Context, Result};
use nvtree::{nvtree_add, nvtree_find, nvtree_pack, nvtree_unpack, Nvtree, Nvtvalue};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

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
    "login",
    "version",
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
    let _terminal = if command == "login" {
        TerminalMode::new(std::io::stdin().as_raw_fd())?
    } else {
        None
    };
    if command == "login" {
        let mut input_stream = stream.try_clone()?;
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
                let mut message = nvtree::nvtree_create(0);
                add_string(&mut message, "event", "stdin");
                add_string(
                    &mut message,
                    "data",
                    &String::from_utf8_lossy(&buffer[..count]),
                );
                if send_message(&mut input_stream, &message).is_err() {
                    break;
                }
            }
        });
    }
    loop {
        let response = receive_message(&mut stream)?;
        if let Some(event) = nvtree_find(&response, "event").map(|pair| &pair.value) {
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
                }
                "base" => {
                    args.push(string_field(request, "name")?.to_string());
                    append_string_option(request, &mut args, "set", "--set")?;
                }
                "vm" => {
                    args.push(string_field(request, "name")?.to_string());
                    args.extend([
                        "--disk".to_string(),
                        string_field(request, "disk")?.to_string(),
                    ]);
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
                }
                _ => bail!("unsupported create type: {resource}"),
            }
        }
        "destroy" => {
            args.push(string_field(request, "type")?.to_string());
            args.push(string_field(request, "name")?.to_string());
        }
        "start" | "stop" | "login" => {
            args.push(string_field(request, "name")?.to_string());
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
            require_arg(&args[1..], 0, "type")
                .map(|value| add_string(&mut request, "type", value))?;
            require_arg(&args[1..], 1, "name")
                .map(|value| add_string(&mut request, "name", value))?;
        }
        "start" | "stop" | "login" => {
            require_arg(&args[1..], 0, "name")
                .map(|value| add_string(&mut request, "name", value))?;
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
        }
        "base" => {
            add_string(request, "name", require_arg(args, 1, "name")?);
            encode_optional_string(request, args, "--set", "set")?;
        }
        "vm" => {
            add_string(request, "name", require_arg(args, 1, "name")?);
            add_string(request, "disk", named_option(args, "--disk")?);
            encode_optional_string(request, args, "--iso", "iso")?;
            encode_optional_number(request, args, "--vnc-port", "vnc_port")?;
            encode_optional_string(request, args, "--vnc-bind", "vnc_bind")?;
            encode_optional_number(request, args, "--vnc-width", "vnc_width")?;
            encode_optional_number(request, args, "--vnc-height", "vnc_height")?;
            add_bool(request, "tpm", has_flag(args, "--tpm"));
            encode_optional_number(request, args, "--cpus", "cpus")?;
            encode_optional_string(request, args, "--memory", "memory")?;
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
