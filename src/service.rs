use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const ANSIBLE_REQUIREMENTS: &str = "- onelove-roles.freebsd-common\n";

#[derive(Deserialize)]
struct Manifest {
    name: String,
    container: String,
    #[serde(default)]
    develop: bool,
    #[serde(default)]
    provisioners: Vec<String>,
}

pub fn init(name: &str, container: &str, provisioner: Option<&str>, develop: bool) -> Result<()> {
    validate_name(name)?;
    if !matches!(container, "jail" | "vm") {
        bail!("unsupported container: {container}");
    }
    if let Some(provisioner) = provisioner {
        if provisioner != "ansible" {
            bail!("unsupported provisioner: {provisioner}");
        }
    }

    let service_dir = PathBuf::from(name);
    if service_dir.exists() {
        bail!("service directory already exists: {name}");
    }
    fs::create_dir(&service_dir)?;

    let provisioners = provisioner
        .map(|value| format!("[\"{value}\"]"))
        .unwrap_or_else(|| "[]".to_string());
    fs::write(
        service_dir.join("service.toml"),
        format!("name = \"{name}\"\ncontainer = \"{container}\"\ndevelop = {develop}\nprovisioners = {provisioners}\n"),
    )?;
    fs::write(
        service_dir.join(".gitignore"),
        "ansible/group_vars/all\nansible/inventory/inventory\nansible/roles/*\n!ansible/roles/.keep\n",
    )?;

    if provisioner == Some("ansible") {
        init_ansible(&service_dir, name)?;
    }

    Ok(())
}

pub fn up_args() -> Result<Vec<String>> {
    let manifest = load_manifest("up")?;
    if manifest.container != "jail" {
        bail!(
            "up currently supports jail services only; {} uses a {} container",
            manifest.name,
            manifest.container
        );
    }
    let mut args = vec![
        "up".to_string(),
        "--container".to_string(),
        manifest.container,
        "--develop".to_string(),
        manifest.develop.to_string(),
        "--service-dir".to_string(),
        std::env::current_dir()?.display().to_string(),
    ];
    for provisioner in manifest.provisioners {
        args.extend(["--provisioner".to_string(), provisioner]);
    }
    args.push(manifest.name);
    Ok(args)
}

pub fn down_args() -> Result<Vec<String>> {
    let manifest = load_manifest("down")?;
    if manifest.container != "jail" {
        bail!(
            "down currently supports jail services only; {} uses a {} container",
            manifest.name,
            manifest.container
        );
    }
    Ok(vec![
        "down".to_string(),
        "--container".to_string(),
        manifest.container,
        "--develop".to_string(),
        manifest.develop.to_string(),
        "--service-dir".to_string(),
        std::env::current_dir()?.display().to_string(),
        manifest.name,
    ])
}

pub fn provision_args() -> Result<Vec<String>> {
    let manifest = load_manifest("provision")?;
    if manifest.provisioners.is_empty() {
        bail!("no provisioner is configured in service.toml");
    }
    if manifest
        .provisioners
        .iter()
        .any(|provisioner| provisioner != "ansible")
    {
        let unsupported = manifest
            .provisioners
            .iter()
            .find(|provisioner| provisioner.as_str() != "ansible")
            .expect("unsupported provisioner exists");
        bail!("unsupported provisioner in service.toml: {unsupported}");
    }
    let mut args = vec![
        "provision".to_string(),
        "--service-dir".to_string(),
        std::env::current_dir()?.display().to_string(),
        "--container".to_string(),
        manifest.container.clone(),
        "--name".to_string(),
        manifest.name.clone(),
    ];
    for provisioner in manifest.provisioners {
        args.extend(["--provisioner".to_string(), provisioner]);
    }
    Ok(args)
}

pub fn provision_at(service_dir: &Path, provisioners: &[String]) -> Result<()> {
    if !service_dir.is_dir() {
        bail!(
            "service directory does not exist: {}",
            service_dir.display()
        );
    }
    if provisioners.is_empty() {
        bail!("no provisioner was supplied");
    }
    for provisioner in provisioners {
        match provisioner.as_str() {
            "ansible" => {
                crate::util::cmd::run_in_dir(
                    "ansible-galaxy",
                    &[
                        "install",
                        "-r",
                        "ansible/requirements.yml",
                        "-p",
                        "ansible/roles",
                    ],
                    service_dir,
                )?;
                crate::util::cmd::run_in_dir(
                    "ansible-playbook",
                    &["-i", "ansible/inventory/inventory", "ansible/site.yml"],
                    service_dir,
                )?;
            }
            other => bail!("unsupported provisioner: {other}"),
        }
    }
    Ok(())
}

pub fn attach_args() -> Result<Vec<String>> {
    let manifest = load_manifest("attach")?;
    Ok(vec!["attach".to_string(), manifest.name])
}

pub fn destroy_args() -> Result<Vec<String>> {
    let manifest = load_manifest("destroy")?;
    if !matches!(manifest.container.as_str(), "jail" | "vm") {
        bail!(
            "unsupported container in service manifest: {}",
            manifest.container
        );
    }
    Ok(vec![
        "destroy".to_string(),
        manifest.container,
        manifest.name,
    ])
}

fn load_manifest(command: &str) -> Result<Manifest> {
    toml::from_str(
        &fs::read_to_string("service.toml").with_context(|| {
            format!("{command} must be run from an initialized service directory")
        })?,
    )
    .context("failed to parse service.toml")
}

fn init_ansible(service_dir: &Path, name: &str) -> Result<()> {
    let ansible_dir = service_dir.join("ansible");
    fs::create_dir_all(ansible_dir.join("group_vars"))?;
    fs::create_dir_all(ansible_dir.join("inventory"))?;
    fs::create_dir_all(ansible_dir.join("roles"))?;
    fs::write(ansible_dir.join("roles/.keep"), "")?;
    fs::write(ansible_dir.join("requirements.yml"), ANSIBLE_REQUIREMENTS)?;
    fs::write(
        ansible_dir.join("site.yml"),
        format!("---\n- name: {name} provisioning\n  hosts: {name}\n  roles: []\n"),
    )?;
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
    {
        bail!("service name must be a single directory name: {name}");
    }
    Ok(())
}
