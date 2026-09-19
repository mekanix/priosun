use crate::config::{BASE_DATASET_PATH, JAIL_BASE, VM_BASE};
use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct ResourceConfig {
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Copy)]
pub enum ResourceKind {
    Jail,
    Vm,
}

fn config_paths() -> Result<Vec<(ResourceKind, String, PathBuf)>> {
    let mut paths = Vec::new();
    for (kind, base) in [(ResourceKind::Jail, JAIL_BASE), (ResourceKind::Vm, VM_BASE)] {
        if !Path::new(base).exists() {
            continue;
        }
        for entry in fs::read_dir(base)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let config = entry.path().join("config.toml");
            if config.exists() {
                if matches!(kind, ResourceKind::Jail)
                    && Path::new(BASE_DATASET_PATH)
                        .join(entry.file_name())
                        .exists()
                {
                    continue;
                }
                paths.push((
                    kind,
                    entry.file_name().to_string_lossy().to_string(),
                    config,
                ));
            }
        }
    }
    Ok(paths)
}

fn graph(overrides: Option<(&str, &[String])>) -> Result<HashMap<String, Vec<String>>> {
    let mut graph = HashMap::new();
    for (_, name, path) in config_paths()? {
        let text = fs::read_to_string(path)?;
        let config: ResourceConfig = toml::from_str(&text)?;
        graph.insert(name, config.dependencies);
    }
    if let Some((name, dependencies)) = overrides {
        graph.insert(name.to_string(), dependencies.to_vec());
    }
    Ok(graph)
}

pub fn validate(name: &str, dependencies: &[String]) -> Result<()> {
    let graph = graph(Some((name, dependencies)))?;
    validate_graph(&graph)
}

pub fn validate_all() -> Result<()> {
    let graph = graph(None)?;
    validate_graph(&graph)
}

fn validate_graph(graph: &HashMap<String, Vec<String>>) -> Result<()> {
    for dependencies in graph.values() {
        for dependency in dependencies {
            if !graph.contains_key(dependency) {
                bail!("dependency does not exist: {dependency}");
            }
        }
    }
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for name in graph.keys() {
        visit(name, graph, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit(
    name: &str,
    graph: &HashMap<String, Vec<String>>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    if visited.contains(name) {
        return Ok(());
    }
    if !visiting.insert(name.to_string()) {
        bail!("cyclic resource dependency involving {name}");
    }
    if let Some(dependencies) = graph.get(name) {
        for dependency in dependencies {
            visit(dependency, graph, visiting, visited)?;
        }
    }
    visiting.remove(name);
    visited.insert(name.to_string());
    Ok(())
}

pub fn set_enabled(path: &Path, enabled: bool) -> Result<()> {
    let text = fs::read_to_string(path)?;
    let mut value: toml::Value = toml::from_str(&text)?;
    value["enabled"] = toml::Value::Boolean(enabled);
    fs::write(path, toml::to_string(&value)?)?;
    Ok(())
}

pub fn remove_references(name: &str) -> Result<()> {
    for (_, _, path) in config_paths()? {
        let text = fs::read_to_string(&path)?;
        let mut value: toml::Value = toml::from_str(&text)?;
        let Some(dependencies) = value.get_mut("dependencies") else {
            continue;
        };
        let Some(array) = dependencies.as_array_mut() else {
            continue;
        };
        let before = array.len();
        array.retain(|dependency| dependency.as_str() != Some(name));
        if array.len() != before {
            fs::write(path, toml::to_string(&value)?)?;
        }
    }
    Ok(())
}

pub fn enabled() -> Result<Vec<(ResourceKind, String)>> {
    let mut resources = Vec::new();
    for (kind, name, path) in config_paths()? {
        let text = fs::read_to_string(path)?;
        let config: ResourceConfig = toml::from_str(&text)?;
        if config.enabled {
            resources.push((kind, name));
        }
    }
    Ok(resources)
}
