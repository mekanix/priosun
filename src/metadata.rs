use anyhow::{Context, Result};
use std::process::{Command, Stdio};

const PREFIX: &str = "priosun:";

fn property(name: &str) -> String {
    format!("{PREFIX}{name}")
}

pub fn set(dataset: &str, name: &str, value: &str) -> Result<()> {
    let property = format!("{}={value}", property(name));
    crate::util::cmd::run("zfs", &["set", &property, dataset])?;
    Ok(())
}

pub fn get(dataset: &str, name: &str) -> Result<Option<String>> {
    let property = property(name);
    let output = Command::new("zfs")
        .args(["get", "-H", "-o", "value", &property, dataset])
        .output()
        .with_context(|| format!("failed to read ZFS property {property} from {dataset}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "-" {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

pub fn get_required(dataset: &str, name: &str) -> Result<String> {
    get(dataset, name)?
        .ok_or_else(|| anyhow::anyhow!("missing ZFS property priosun:{name} on {dataset}"))
}

pub fn get_bool(dataset: &str, name: &str, default: bool) -> Result<bool> {
    match get(dataset, name)? {
        Some(value) => value
            .parse()
            .with_context(|| format!("invalid boolean ZFS property priosun:{name}={value}")),
        None => Ok(default),
    }
}

pub fn dataset_exists(dataset: &str) -> bool {
    Command::new("zfs")
        .args(["list", "-H", "-o", "name", dataset])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
