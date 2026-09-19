use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::thread;
use tracing::debug;

#[derive(Debug, Default)]
pub struct Transcript {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

thread_local! {
    static TRANSCRIPT: RefCell<Option<Transcript>> = const { RefCell::new(None) };
    static STREAM: RefCell<Option<StreamCallback>> = const { RefCell::new(None) };
}

type StreamCallback = Arc<dyn Fn(CommandEvent) + Send + Sync>;

#[derive(Debug)]
pub enum CommandStream {
    Command,
    Stdout,
    Stderr,
    Message,
}

#[derive(Debug)]
pub struct CommandEvent {
    pub program: String,
    pub args: Vec<String>,
    pub stream: CommandStream,
    pub data: Vec<u8>,
}

pub fn message(message: &str) {
    STREAM.with(|stream| {
        if let Some(callback) = stream.borrow().as_ref() {
            callback(CommandEvent {
                program: String::new(),
                args: Vec::new(),
                stream: CommandStream::Message,
                data: message.as_bytes().to_vec(),
            });
        }
    });
}

pub fn with_stream<T>(
    callback: Arc<dyn Fn(CommandEvent) + Send + Sync>,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    STREAM.with(|stream| {
        *stream.borrow_mut() = Some(callback);
        let result = operation();
        *stream.borrow_mut() = None;
        result
    })
}

pub fn capture<T>(operation: impl FnOnce() -> Result<T>) -> Result<(T, Transcript)> {
    TRANSCRIPT.with(|transcript| {
        *transcript.borrow_mut() = Some(Transcript::default());
        let result = operation();
        let captured = transcript.borrow_mut().take().unwrap_or_default();
        result.map(|value| (value, captured))
    })
}

pub fn run(program: &str, args: &[&str]) -> Result<Output> {
    debug!("Running: {} {}", program, args.join(" "));
    let callback = STREAM.with(|stream| stream.borrow().clone());
    if let Some(callback) = callback {
        return run_streaming(program, args, callback);
    }
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {} {}", program, args.join(" ")))?;
    TRANSCRIPT.with(|transcript| {
        if let Some(transcript) = transcript.borrow_mut().as_mut() {
            transcript
                .stdout
                .extend(format!("$ {} {}\n", program, args.join(" ")).as_bytes());
            transcript.stdout.extend(&output.stdout);
            transcript.stderr.extend(&output.stderr);
        }
    });
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "command failed: {} {} (exit code: {:?})\nstderr: {}",
            program,
            args.join(" "),
            output.status.code(),
            stderr.trim()
        );
    }
    Ok(output)
}

pub fn run_append_log(program: &str, args: &[&str], log_path: &Path) -> Result<()> {
    debug!("Running: {} {}", program, args.join(" "));
    if let Some(callback) = STREAM.with(|stream| stream.borrow().clone()) {
        callback(CommandEvent {
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            stream: CommandStream::Command,
            data: Vec::new(),
        });
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open log {}", log_path.display()))?;
    let stderr = stdout.try_clone()?;
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .with_context(|| format!("failed to execute {} {}", program, args.join(" ")))?;
    if !status.success() {
        bail!(
            "command failed: {} {} (exit code: {:?}); see {}",
            program,
            args.join(" "),
            status.code(),
            log_path.display()
        );
    }
    Ok(())
}

fn run_streaming(
    program: &str,
    args: &[&str],
    callback: Arc<dyn Fn(CommandEvent) + Send + Sync>,
) -> Result<Output> {
    callback(CommandEvent {
        program: program.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        stream: CommandStream::Command,
        data: Vec::new(),
    });
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to execute {} {}", program, args.join(" ")))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_callback = Arc::clone(&callback);
    let stderr_callback = Arc::clone(&callback);
    let stdout_thread = thread::spawn(move || read_stream(stdout, stdout_callback, true));
    let stderr_thread = thread::spawn(move || read_stream(stderr, stderr_callback, false));
    let status = child.wait()?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stdout streaming thread panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stderr streaming thread panicked"))??;
    if !status.success() {
        bail!(
            "command failed: {} {} (exit code: {:?})\nstderr: {}",
            program,
            args.join(" "),
            status.code(),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_stream<R: Read>(
    mut reader: R,
    callback: Arc<dyn Fn(CommandEvent) + Send + Sync>,
    stdout: bool,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let data = buffer[..count].to_vec();
        output.extend(&data);
        callback(CommandEvent {
            program: String::new(),
            args: Vec::new(),
            stream: if stdout {
                CommandStream::Stdout
            } else {
                CommandStream::Stderr
            },
            data,
        });
    }
    Ok(output)
}
