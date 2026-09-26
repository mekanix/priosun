use anyhow::{bail, Context, Result};
use nvtree::{nvtree_add, nvtree_find, nvtree_string, Nvtvalue};
use priosun::protocol::{self, SOCKET_BASE, SOCKET_PATH};
use priosun::{
    bhyve,
    config::{
        Config, BASE_DATASET_PATH, BASE_PATH, IMAGE_BASE, JAIL_BASE, LOG_BASE, SEED_BASE, VM_BASE,
    },
    dependency, jail, metadata, network, service,
    util::cmd,
};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

fn client_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .parent()
        .context("priosund has no executable directory")?
        .join("priosun"))
}

fn is_client_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| {
                code == libc::EPIPE || code == libc::ECONNRESET || code == libc::ENOTCONN
            })
    })
}

fn config_path() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2)
        .find(|args| args[0] == "--config")
        .map(|args| args[1].clone())
}

fn ensure_datasets(config: &Config) -> Result<()> {
    let var_dataset = format!("{}{}", config.zfs_pool, "/var");
    if !dataset_exists(&var_dataset)? {
        cmd::run("zfs", &["create", "-o", "mountpoint=none", &var_dataset])?;
    }

    let base_dataset = format!("{}{}", config.zfs_pool, BASE_PATH);
    if !dataset_exists(&base_dataset)? {
        cmd::run(
            "zfs",
            &["create", "-o", "mountpoint=/var/priosun", &base_dataset],
        )?;
    }
    cmd::run("zfs", &["set", "compression=lz4", &base_dataset])?;

    for path in [BASE_DATASET_PATH, JAIL_BASE, VM_BASE, SEED_BASE, IMAGE_BASE] {
        let dataset = format!("{}{}", config.zfs_pool, path);
        if !dataset_exists(&dataset)? {
            cmd::run("zfs", &["create", &dataset])?;
        }
    }
    Ok(())
}

fn dataset_exists(dataset: &str) -> Result<bool> {
    Command::new("zfs")
        .args(["list", "-H", "-o", "name", dataset])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to check ZFS dataset {dataset}"))
        .map(|status| status.success())
}

fn start_enabled(config: &Config) -> Result<()> {
    dependency::validate_all(config)?;
    for (kind, name) in dependency::enabled(config)? {
        match kind {
            dependency::ResourceKind::Jail => jail::start(&name, config)?,
            dependency::ResourceKind::Vm => bhyve::start(&name, config)?,
        }
    }
    Ok(())
}

fn handle(mut stream: UnixStream) -> Result<()> {
    let request = protocol::receive_request(&mut stream)?;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    if matches!(
        nvtree_find(&request, "command").map(|pair| &pair.value),
        Some(Nvtvalue::String(command)) if command == "provision"
    ) {
        let config = Config::load()?;
        let result = execute_provision_request(&request, &config);
        let response = match result {
            Ok((stdout, stderr)) => protocol::success(0, &stdout, &stderr),
            Err(error) => protocol::error(&format!("{error:#}")),
        };
        protocol::send_response(
            &mut writer.lock().expect("response writer lock poisoned"),
            &response,
        )?;
        return Ok(());
    }
    if matches!(
        nvtree_find(&request, "command").map(|pair| &pair.value),
        Some(Nvtvalue::String(command)) if command == "network-init"
    ) {
        let config = Config::load()?;
        let event_writer = Arc::clone(&writer);
        let callback = Arc::new(move |event: priosun::util::cmd::CommandEvent| {
            let response = match event.stream {
                priosun::util::cmd::CommandStream::Message => {
                    protocol::message_event(&String::from_utf8_lossy(&event.data))
                }
                priosun::util::cmd::CommandStream::Command => protocol::command_event(
                    &format!("{} {}", event.program, event.args.join(" ")),
                    None,
                    None,
                ),
                priosun::util::cmd::CommandStream::Stdout => {
                    protocol::command_event("", Some(&event.data), None)
                }
                priosun::util::cmd::CommandStream::Stderr => {
                    protocol::command_event("", None, Some(&event.data))
                }
            };
            if let Ok(mut writer) = event_writer.lock() {
                let _ = protocol::send_response(&mut writer, &response);
            }
        });
        let result = priosun::util::cmd::with_stream(callback, || network::init(&config));
        let response = match result {
            Ok(()) => protocol::success(0, &[], &[]),
            Err(error) => protocol::error(&format!("{error:#}")),
        };
        protocol::send_response(
            &mut writer.lock().expect("response writer lock poisoned"),
            &response,
        )?;
        return Ok(());
    }
    let command = matches!(
        nvtree_find(&request, "command").map(|pair| &pair.value),
        Some(Nvtvalue::String(command)) if command == "start" || command == "create"
    );
    let start_attach = command
        && optional_bool(&request, "attach").unwrap_or(false)
        && (optional_bool(&request, "start").unwrap_or(false)
            || matches!(
                nvtree_find(&request, "command").map(|pair| &pair.value),
                Some(Nvtvalue::String(command)) if command == "start"
            ));
    if start_attach {
        let result = (|| {
            execute_jail_request(&request, &writer)?.context("failed to start resource")?;
            execute_attach(&request, stream, Arc::clone(&writer))
        })();
        if let Err(error) = result {
            protocol::send_response(
                &mut writer.lock().expect("response writer lock poisoned"),
                &protocol::error(&format!("{:#}", error)),
            )?;
        }
        return Ok(());
    }
    if matches!(
        nvtree_find(&request, "command").map(|pair| &pair.value),
        Some(Nvtvalue::String(command)) if command == "attach"
    ) {
        return execute_attach(&request, stream, writer);
    }
    let result = (|| -> Result<_> {
        let response = if let Some(response) = execute_jail_request(&request, &writer)? {
            response
        } else {
            let args = protocol::request_to_args(&request)?;
            let output = Command::new(client_binary()?)
                .args(args)
                .args(
                    config_path()
                        .map(|path| vec!["--config".to_string(), path])
                        .unwrap_or_default(),
                )
                .env("PRIOSUN_DAEMON_EXEC", "1")
                .output()
                .context("failed to execute priosun command")?;
            protocol::success(
                output.status.code().unwrap_or(1),
                &output.stdout,
                &output.stderr,
            )
        };
        Ok(response)
    })();
    let response = match result {
        Ok(response) => response,
        Err(error) => protocol::error(&format!("{:#}", error)),
    };
    protocol::send_response(
        &mut writer.lock().expect("response writer lock poisoned"),
        &response,
    )?;
    Ok(())
}

fn execute_provision_request(
    request: &nvtree::Nvtree,
    config: &Config,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let service_dir = Path::new(string_field(request, "service_dir")?);
    let provisioners = string_array_field(request, "provisioners")?;
    let (_, transcript) = cmd::capture(|| {
        provision_service(
            service_dir,
            string_field(request, "container")?,
            string_field(request, "name")?,
            provisioners,
            config,
        )
    })?;
    Ok((transcript.stdout, transcript.stderr))
}

fn provision_service(
    service_dir: &Path,
    container: &str,
    name: &str,
    provisioners: &[String],
    config: &Config,
) -> Result<()> {
    service::provision_at(service_dir, provisioners)?;
    let dataset = service_dataset(container, name, config)?;
    metadata::set(&dataset, "provisioned", "true")?;
    Ok(())
}

fn service_dataset(container: &str, name: &str, config: &Config) -> Result<String> {
    let base = match container {
        "jail" => JAIL_BASE,
        "vm" => VM_BASE,
        other => bail!("unsupported container: {other}"),
    };
    Ok(format!(
        "{}{}",
        config.zfs_pool,
        Path::new(base).join(name).display()
    ))
}

fn execute_attach(
    request: &nvtree::Nvtree,
    input_stream: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
) -> Result<()> {
    let name = string_field(request, "name")?;
    let config = Config::load()?;
    if bhyve::path_exists(name, &config) {
        return execute_vm_attach(name, input_stream, writer, &config);
    }
    execute_jail_attach(name, input_stream, writer)
}

fn execute_jail_attach(
    name: &str,
    input_stream: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
) -> Result<()> {
    let running = ::jail::RunningJail::from_name(name)
        .map_err(|error| anyhow::anyhow!("failed to find jail {name}: {error}"))?;
    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        bail!("posix_openpt: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::grantpt(master_fd) } < 0 || unsafe { libc::unlockpt(master_fd) } < 0 {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(master_fd) };
        bail!("prepare PTY: {error}");
    }
    let slave_name = unsafe { std::ffi::CStr::from_ptr(libc::ptsname(master_fd)) }.to_owned();
    let slave_fd = unsafe { libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave_fd < 0 {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(master_fd) };
        bail!("open PTY slave: {error}");
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        bail!("fork: {}", std::io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::close(master_fd);
            if libc::setsid() < 0
                || libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0
                || libc::dup2(slave_fd, libc::STDIN_FILENO) < 0
                || libc::dup2(slave_fd, libc::STDOUT_FILENO) < 0
                || libc::dup2(slave_fd, libc::STDERR_FILENO) < 0
            {
                libc::_exit(126);
            }
            libc::close(slave_fd);
            if libc::jail_attach(running.jid) < 0 {
                libc::_exit(126);
            }
            let login = std::ffi::CString::new("login").expect("literal has no nul");
            let force = std::ffi::CString::new("-f").expect("literal has no nul");
            let root = std::ffi::CString::new("root").expect("literal has no nul");
            let mut argv = [
                login.as_ptr(),
                force.as_ptr(),
                root.as_ptr(),
                std::ptr::null(),
            ];
            libc::execvp(login.as_ptr(), argv.as_mut_ptr());
            libc::_exit(127);
        }
    }
    unsafe { libc::close(slave_fd) };
    let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let child_input = master.try_clone()?;
    protocol::send_response(
        &mut writer.lock().expect("response writer lock poisoned"),
        &protocol::command_event("login -f root", None, None),
    )?;
    let output_writer = Arc::clone(&writer);
    let output_thread = thread::spawn(move || relay_login_output(master, output_writer));
    thread::spawn(move || relay_login_input(input_stream, child_input));
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let _ = output_thread.join();
    let exit_code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1
    };
    protocol::send_response(
        &mut writer.lock().expect("response writer lock poisoned"),
        &protocol::success(exit_code, &[], &[]),
    )?;
    Ok(())
}

fn execute_vm_attach(
    name: &str,
    input_stream: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
    config: &Config,
) -> Result<()> {
    if !bhyve::is_running(name, config) {
        bail!("VM is not running: {name}");
    }
    let serial = bhyve::serial_console(name, config)?;
    protocol::send_response(
        &mut writer.lock().expect("response writer lock poisoned"),
        &protocol::serial_console_event(),
    )?;
    let child_input = serial.try_clone()?;
    let output_writer = Arc::clone(&writer);
    let output_thread = thread::spawn(move || relay_login_output(serial, output_writer));
    thread::spawn(move || relay_login_input(input_stream, child_input));
    let _ = output_thread.join();
    protocol::send_response(
        &mut writer.lock().expect("response writer lock poisoned"),
        &protocol::success(0, &[], &[]),
    )?;
    Ok(())
}

fn relay_login_input(mut stream: UnixStream, mut child_stdin: std::fs::File) -> Result<()> {
    loop {
        let message = protocol::receive_message(&mut stream)?;
        match nvtree_find(&message, "event").map(|pair| &pair.value) {
            Some(Nvtvalue::String(event)) if event == "stdin" => {
                if let Some(Nvtvalue::String(data)) =
                    nvtree_find(&message, "data").map(|pair| &pair.value)
                {
                    child_stdin.write_all(data.as_bytes())?;
                }
            }
            Some(Nvtvalue::String(event)) if event == "stdin_eof" => break,
            _ => {}
        }
    }
    Ok(())
}

fn relay_login_output(mut input: std::fs::File, writer: Arc<Mutex<UnixStream>>) -> Result<()> {
    let mut buffer = [0_u8; 4096];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let event = protocol::command_event("", Some(&buffer[..count]), None);
        protocol::send_response(
            &mut writer.lock().expect("response writer lock poisoned"),
            &event,
        )?;
    }
    Ok(())
}

fn execute_jail_request(
    request: &nvtree::Nvtree,
    writer: &Arc<Mutex<UnixStream>>,
) -> Result<Option<nvtree::Nvtree>> {
    let command = match nvtree_find(request, "command").map(|pair| &pair.value) {
        Some(Nvtvalue::String(value)) => value.as_str(),
        _ => return Ok(None),
    };
    if command == "list" {
        return Ok(Some(execute_list(request)?));
    }
    let config = Config::load()?;
    let is_jail = match command {
        "create" => {
            let resource_type = string_field(request, "type").ok();
            let base_resource = string_field(request, "resource").ok();
            resource_type == Some("jail")
                || (resource_type == Some("base") && base_resource == Some("jail"))
        }
        "start" | "stop" | "attach" => nvtree_find(request, "name")
            .and_then(|pair| match &pair.value {
                Nvtvalue::String(name) => Some(jail::path_exists(name, &config)),
                _ => None,
            })
            .unwrap_or(false),
        "destroy" => matches!(
            nvtree_find(request, "type").map(|pair| &pair.value),
            Some(Nvtvalue::String(value)) if value == "jail" || value == "base"
        ),
        "up" | "down" => true,
        _ => false,
    };
    let is_vm = match command {
        "create" => matches!(
            nvtree_find(request, "type").map(|pair| &pair.value),
            Some(Nvtvalue::String(value)) if value == "vm"
        ),
        "start" | "stop" => nvtree_find(request, "name")
            .and_then(|pair| match &pair.value {
                Nvtvalue::String(name) => Some(bhyve::path_exists(name, &config)),
                _ => None,
            })
            .unwrap_or(false),
        "destroy" => matches!(
            nvtree_find(request, "type").map(|pair| &pair.value),
            Some(Nvtvalue::String(value)) if value == "vm"
        ),
        _ => false,
    };
    if !is_jail && !is_vm {
        return Ok(None);
    }

    let event_writer = Arc::clone(writer);
    let callback = Arc::new(move |event: priosun::util::cmd::CommandEvent| {
        let command = match event.stream {
            priosun::util::cmd::CommandStream::Command => {
                format!("{} {}", event.program, event.args.join(" "))
            }
            priosun::util::cmd::CommandStream::Stdout => String::new(),
            priosun::util::cmd::CommandStream::Stderr => String::new(),
            priosun::util::cmd::CommandStream::Message => String::new(),
        };
        let response = match event.stream {
            priosun::util::cmd::CommandStream::Command => {
                protocol::command_event(&command, None, None)
            }
            priosun::util::cmd::CommandStream::Stdout => {
                protocol::command_event("", Some(&event.data), None)
            }
            priosun::util::cmd::CommandStream::Stderr => {
                protocol::command_event("", None, Some(&event.data))
            }
            priosun::util::cmd::CommandStream::Message => {
                protocol::message_event(&String::from_utf8_lossy(&event.data))
            }
        };
        if let Ok(mut writer) = event_writer.lock() {
            let _ = protocol::send_response(&mut writer, &response);
        }
    });
    priosun::util::cmd::with_stream(callback, || {
        match command {
            "up" => {
                let name = string_field(request, "name")?;
                let develop = bool_field(request, "develop")?;
                let running = ::jail::RunningJail::from_name(name).is_ok();
                if !jail::path_exists(name, &config) {
                    jail::create(name, None, None, false, None, None, &config)?;
                }
                jail::set_enabled(name, !develop, &config)?;
                if develop && !running {
                    mount_development_dir(name, string_field(request, "service_dir")?)?;
                }
                jail::start(name, &config)?;
                let dataset = service_dataset("jail", name, &config)?;
                if !metadata::get_bool(&dataset, "provisioned", false)? {
                    let provisioners = string_array_field(request, "provisioners")?;
                    if !provisioners.is_empty() {
                        provision_service(
                            Path::new(string_field(request, "service_dir")?),
                            "jail",
                            name,
                            provisioners,
                            &config,
                        )?;
                    }
                }
            }
            "down" => {
                let name = string_field(request, "name")?;
                jail::stop(name, &config)?;
                if bool_field(request, "develop")? {
                    unmount_development_dir(name);
                }
            }
            "create" => {
                let name = string_field(request, "name")?;
                let resource_type = string_field(request, "type")?;
                let resource = string_field(request, "resource").unwrap_or(resource_type);
                if resource_type == "vm" {
                    create_vm(request, name, &config)?;
                    if optional_bool(request, "start") == Some(true)
                        || optional_bool(request, "cloud_init") == Some(true)
                    {
                        if optional_bool(request, "cloud_init") == Some(true) {
                            bhyve::start_with_seed(name, &config)?;
                        } else {
                            bhyve::start(name, &config)?;
                        }
                    }
                } else {
                    if resource_type == "base" && resource != "jail" {
                        bail!("base {resource} creation is not supported yet");
                    }
                    let set = optional_string(request, "set");
                    let version = optional_string(request, "version");
                    let base = resource_type == "base";
                    let from_base = optional_string(request, "base");
                    jail::create(
                        name,
                        set,
                        version,
                        base,
                        from_base,
                        optional_string(request, "ssh_key"),
                        &config,
                    )?;
                    if !base && optional_bool(request, "start") == Some(true) {
                        jail::start(name, &config)?;
                    }
                }
            }
            "start" => {
                if is_vm {
                    bhyve::start(string_field(request, "name")?, &config)?;
                } else {
                    jail::start(string_field(request, "name")?, &config)?;
                }
            }
            "stop" => {
                if is_vm {
                    bhyve::stop(string_field(request, "name")?, &config)?;
                } else {
                    jail::stop(string_field(request, "name")?, &config)?;
                }
            }
            "destroy" => {
                let resource = string_field(request, "type")?;
                if resource == "vm" {
                    bhyve::destroy(string_field(request, "name")?, &config)?;
                } else if resource == "base" {
                    jail::destroy_base(string_field(request, "name")?, &config)?;
                } else {
                    jail::destroy(string_field(request, "name")?, &config)?;
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    })?;
    Ok(Some(protocol::success(0, &[], &[])))
}

fn create_vm(request: &nvtree::Nvtree, name: &str, config: &Config) -> Result<()> {
    let vnc_port = optional_number(request, "vnc_port")
        .map(|value| u16::try_from(value).context("vnc_port is out of range"))
        .transpose()?;
    let vnc_width = optional_number(request, "vnc_width")
        .map(|value| u32::try_from(value).context("vnc_width is out of range"))
        .transpose()?
        .unwrap_or(1024);
    let vnc_height = optional_number(request, "vnc_height")
        .map(|value| u32::try_from(value).context("vnc_height is out of range"))
        .transpose()?
        .unwrap_or(768);
    let cpus = optional_number(request, "cpus")
        .map(|value| u32::try_from(value).context("cpus is out of range"))
        .transpose()?
        .unwrap_or(1);
    let vnc_bind = optional_string(request, "vnc_bind").unwrap_or("127.0.0.1");
    let memory = optional_string(request, "memory").unwrap_or("1G");
    bhyve::create(
        name,
        &bhyve::VmCreateOptions {
            disk: string_field(request, "disk")?,
            os: string_field(request, "os")?,
            version: optional_string(request, "version"),
            cloud_init: bool_field(request, "cloud_init")?,
            ssh_key: optional_string(request, "ssh_key"),
            iso: optional_string(request, "iso"),
            vnc_port,
            vnc_bind,
            vnc_width,
            vnc_height,
            tpm: bool_field(request, "tpm")?,
            cpus,
            memory,
        },
        config,
    )
}

fn mount_development_dir(name: &str, service_dir: &str) -> Result<()> {
    let source = Path::new(service_dir);
    if !source.is_dir() {
        bail!("development service directory does not exist: {service_dir}");
    }
    let target = Path::new(JAIL_BASE).join(name).join("usr/src");
    fs::create_dir_all(&target)?;
    let source = source.display().to_string();
    let target = target.display().to_string();
    cmd::message(&format!("Mounting {source} at {target}"));
    cmd::run("mount", &["-t", "nullfs", &source, &target])?;
    Ok(())
}

fn unmount_development_dir(name: &str) {
    let target = Path::new(JAIL_BASE).join(name).join("usr/src");
    let _ = cmd::run("umount", &[&target.display().to_string()]);
}

fn optional_string<'a>(request: &'a nvtree::Nvtree, name: &str) -> Option<&'a str> {
    nvtree_find(request, name).and_then(|pair| match &pair.value {
        Nvtvalue::String(value) => Some(value.as_str()),
        _ => None,
    })
}

fn optional_bool(request: &nvtree::Nvtree, name: &str) -> Option<bool> {
    nvtree_find(request, name).and_then(|pair| match &pair.value {
        Nvtvalue::Bool(value) => Some(*value),
        _ => None,
    })
}

fn optional_number(request: &nvtree::Nvtree, name: &str) -> Option<u64> {
    nvtree_find(request, name).and_then(|pair| match &pair.value {
        Nvtvalue::Number(value) => Some(*value),
        _ => None,
    })
}

fn bool_field(request: &nvtree::Nvtree, name: &str) -> Result<bool> {
    match nvtree_find(request, name).map(|pair| &pair.value) {
        Some(Nvtvalue::Bool(value)) => Ok(*value),
        _ => bail!("request is missing boolean field {name}"),
    }
}

fn string_field<'a>(request: &'a nvtree::Nvtree, name: &str) -> Result<&'a str> {
    match nvtree_find(request, name).map(|pair| &pair.value) {
        Some(Nvtvalue::String(value)) => Ok(value),
        _ => bail!("request is missing string field {name}"),
    }
}

fn string_array_field<'a>(request: &'a nvtree::Nvtree, name: &str) -> Result<&'a [String]> {
    match nvtree_find(request, name).map(|pair| &pair.value) {
        Some(Nvtvalue::StringArray(value)) => Ok(value),
        _ => bail!("request is missing string array field {name}"),
    }
}

fn execute_list(request: &nvtree::Nvtree) -> Result<nvtree::Nvtree> {
    let resource = string_field(request, "resource")?;
    if resource == "all" {
        bail!("list all must be expanded by the client");
    }
    let rows = match resource {
        "datasets" => zfs_rows(&["list", "-H", "-o", "name"])?,
        "volumes" => zfs_rows(&["list", "-H", "-o", "name", "-t", "volume"])?,
        "jails" => jail::list(&Config::load()?)?
            .into_iter()
            .map(|jail| {
                let mut row = nvtree::nvtree_create(0);
                nvtree_add(&mut row, nvtree_string("name", &jail.name));
                nvtree_add(&mut row, nvtree_string("hostname", &jail.hostname));
                nvtree_add(&mut row, nvtree_string("ips", &jail.ips));
                nvtree_add(&mut row, nvtree_string("status", &jail.status));
                row
            })
            .collect(),
        "vms" => {
            let config = Config::load()?;
            bhyve::list(&config)?
                .into_iter()
                .map(|(name, running)| {
                    let mut row = nvtree::nvtree_create(0);
                    nvtree_add(&mut row, nvtree_string("name", &name));
                    nvtree_add(
                        &mut row,
                        nvtree_string("status", if running { "running" } else { "stopped" }),
                    );
                    row
                })
                .collect()
        }
        _ => bail!("unsupported list resource: {resource}"),
    };
    Ok(match resource {
        "datasets" => protocol::datasets_success(rows),
        "volumes" => protocol::volumes_success(rows),
        "jails" => protocol::jails_success(rows),
        "vms" => protocol::vms_success(rows),
        _ => unreachable!(),
    })
}

fn zfs_rows(args: &[&str]) -> Result<Vec<nvtree::Nvtree>> {
    let output = Command::new("zfs")
        .args(args)
        .output()
        .context("failed to execute zfs")?;
    if !output.status.success() {
        bail!(
            "zfs failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|name| !name.trim().is_empty())
        .map(|name| {
            let mut row = nvtree::nvtree_create(0);
            nvtree_add(&mut row, nvtree_string("name", name.trim()));
            row
        })
        .collect())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let foreground = std::env::args().any(|arg| arg == "--no-daemon");
    if !foreground {
        let mut daemon = Command::new(std::env::current_exe()?);
        daemon.arg("--no-daemon");
        if let Some(path) = config_path() {
            daemon.args(["--config", &path]);
        }
        daemon
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("failed to daemonize priosund")?;
        return Ok(());
    }
    let config = Config::load()?;
    fs::create_dir_all(LOG_BASE).with_context(|| format!("failed to create {}", LOG_BASE))?;
    ensure_datasets(&config)?;
    start_enabled(&config)?;
    let socket = PathBuf::from(SOCKET_PATH);
    fs::create_dir_all(SOCKET_BASE).with_context(|| format!("failed to create {}", SOCKET_BASE))?;
    let _ = fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("failed to bind {}", SOCKET_PATH))?;
    fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o660))?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(|| {
                    if let Err(error) = handle(stream) {
                        if !is_client_disconnect(&error) {
                            tracing::error!(%error, "request failed");
                        }
                    }
                });
            }
            Err(error) => tracing::error!(%error, "socket accept failed"),
        }
    }
    Ok(())
}
