use crate::config::{Config, BASE_DATASET_PATH, JAIL_BASE, VM_BASE};
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub enum ResourceKind {
    Jail,
    Vm,
}

fn resources(config: &Config) -> Result<Vec<(ResourceKind, String, String)>> {
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
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(kind, ResourceKind::Jail)
                && Path::new(BASE_DATASET_PATH).join(&name).exists()
            {
                continue;
            }
            paths.push((
                kind,
                name,
                format!("{}{}", config.zfs_pool, entry.path().display()),
            ));
        }
    }
    Ok(paths)
}

fn graph(
    config: &Config,
    overrides: Option<(&str, &[String])>,
) -> Result<HashMap<String, Vec<String>>> {
    let mut graph = HashMap::new();
    for (_, name, dataset) in resources(config)? {
        let dependencies = crate::metadata::get(&dataset, "dependencies")?
            .unwrap_or_default()
            .split(',')
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect();
        graph.insert(name, dependencies);
    }
    if let Some((name, dependencies)) = overrides {
        graph.insert(name.to_string(), dependencies.to_vec());
    }
    Ok(graph)
}

pub fn validate(name: &str, dependencies: &[String], config: &Config) -> Result<()> {
    let graph = graph(config, Some((name, dependencies)))?;
    validate_graph(&graph)
}

pub fn validate_all(config: &Config) -> Result<()> {
    let graph = graph(config, None)?;
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

pub fn remove_references(name: &str, config: &Config) -> Result<()> {
    for (_, _, dataset) in resources(config)? {
        let current = crate::metadata::get(&dataset, "dependencies")?
            .unwrap_or_default()
            .split(',')
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let filtered = current
            .iter()
            .filter(|dependency| dependency.as_str() != name)
            .cloned()
            .collect::<Vec<_>>();
        if filtered.len() != current.len() {
            crate::metadata::set(&dataset, "dependencies", &filtered.join(","))?;
        }
    }
    Ok(())
}

pub fn enabled(config: &Config) -> Result<Vec<(ResourceKind, String)>> {
    let mut enabled_resources = Vec::new();
    for (kind, name, dataset) in resources(config)? {
        if crate::metadata::get_bool(&dataset, "enabled", true)? {
            enabled_resources.push((kind, name));
        }
    }
    Ok(enabled_resources)
}
