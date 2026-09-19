use anyhow::{bail, Context, Result};
use nvtree::{nvtree_add, nvtree_find, nvtree_string, Nvtvalue};
use priosun::protocol::{self, SOCKET_BASE, SOCKET_PATH};
use priosun::{
    bhyve,
    config::{Config, BASE_DATASET_PATH, BASE_PATH, JAIL_BASE, LOG_BASE, VM_BASE},
    dependency, jail,
    util::cmd,
};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
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

    for path in [BASE_DATASET_PATH, JAIL_BASE, VM_BASE] {
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
    dependency::validate_all()?;
    for (kind, name) in dependency::enabled()? {
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
        Some(Nvtvalue::String(command)) if command == "login"
    ) {
        return execute_login(&request, stream, writer);
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

fn execute_login(
    request: &nvtree::Nvtree,
    input_stream: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
) -> Result<()> {
    let name = string_field(request, "name")?;
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
        "create" => matches!(
            nvtree_find(request, "type").map(|pair| &pair.value),
            Some(Nvtvalue::String(value)) if value == "jail" || value == "base"
        ),
        "start" | "stop" | "login" => nvtree_find(request, "name")
            .and_then(|pair| match &pair.value {
                Nvtvalue::String(name) => Some(jail::path_exists(name, &config)),
                _ => None,
            })
            .unwrap_or(false),
        "destroy" => matches!(
            nvtree_find(request, "type").map(|pair| &pair.value),
            Some(Nvtvalue::String(value)) if value == "jail"
        ),
        _ => false,
    };
    if !is_jail {
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
            "create" => {
                let name = string_field(request, "name")?;
                let set = nvtree_find(request, "set").and_then(|pair| match &pair.value {
                    Nvtvalue::String(value) => Some(value.as_str()),
                    _ => None,
                });
                let base = matches!(
                    nvtree_find(request, "type").map(|pair| &pair.value),
                    Some(Nvtvalue::String(value)) if value == "base"
                );
                let from_base = nvtree_find(request, "base").and_then(|pair| match &pair.value {
                    Nvtvalue::String(value) => Some(value.as_str()),
                    _ => None,
                });
                jail::create(name, set, base, from_base, &config)?;
            }
            "start" => jail::start(string_field(request, "name")?, &config)?,
            "stop" => jail::stop(string_field(request, "name")?, &config)?,
            "destroy" => jail::destroy(string_field(request, "name")?, &config)?,
            _ => unreachable!(),
        }
        Ok(())
    })?;
    Ok(Some(protocol::success(0, &[], &[])))
}

fn string_field<'a>(request: &'a nvtree::Nvtree, name: &str) -> Result<&'a str> {
    match nvtree_find(request, name).map(|pair| &pair.value) {
        Some(Nvtvalue::String(value)) => Ok(value),
        _ => bail!("request is missing string field {name}"),
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
        "jails" => jail::list()?
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
                        tracing::error!(%error, "request failed");
                    }
                });
            }
            Err(error) => tracing::error!(%error, "socket accept failed"),
        }
    }
    Ok(())
}
