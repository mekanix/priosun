use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use nvtree::{nvtree_find, Nvtvalue};
use priosun::{bhyve, config, jail, network, output, protocol, service, util};
use std::io::Write;
use tabled::Tabled;

#[derive(Parser)]
#[command(name = "priosun")]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    #[arg(long = "config", global = true, hide = true)]
    _config: Option<std::path::PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Create {
        #[command(subcommand)]
        resource: CreateResource,
    },
    Destroy {
        #[command(subcommand)]
        resource: Option<DestroyResource>,
    },
    Start {
        name: String,
        #[arg(long)]
        attach: bool,
    },
    Stop {
        name: String,
    },
    Attach {
        name: Option<String>,
    },
    Dependencies {
        resource: DependencyResource,
        name: String,
        dependencies: String,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Version,
    NetworkInit,
    Up,
    Down,
    Provision,
    Init {
        #[arg(long, value_enum)]
        provisioner: Option<Provisioner>,
        #[arg(long, value_enum, default_value_t = Container::Jail)]
        container: Container,
        #[arg(long)]
        develop: bool,
        name: String,
    },
    List {
        resource: ListResource,
    },
}

#[derive(Clone, ValueEnum)]
enum Provisioner {
    Ansible,
}

#[derive(Clone, ValueEnum)]
enum Container {
    Jail,
    Vm,
}

#[derive(Clone, ValueEnum)]
enum ListResource {
    Datasets,
    Volumes,
    Jails,
    Vms,
    All,
}

#[derive(Tabled)]
struct ResourceRow {
    kind: String,
    name: String,
    status: String,
}

#[derive(Subcommand)]
enum CreateResource {
    Dataset {
        name: String,
    },
    Volume {
        name: String,
        #[arg(long)]
        size: String,
    },
    Jail {
        name: String,
        #[arg(long)]
        set: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        ssh_key: Option<String>,
        #[arg(long)]
        start: bool,
        #[arg(long, requires = "start")]
        attach: bool,
    },
    Base {
        #[command(subcommand)]
        resource: BaseResource,
    },
    Vm {
        name: String,
        #[arg(long)]
        disk: String,
        #[arg(long)]
        os: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, conflicts_with = "iso")]
        cloud_init: bool,
        #[arg(long, conflicts_with = "cloud_init")]
        iso: Option<String>,
        #[arg(long, requires = "cloud_init")]
        ssh_key: Option<String>,
        #[arg(long)]
        vnc_port: Option<u16>,
        #[arg(long, default_value = "127.0.0.1")]
        vnc_bind: String,
        #[arg(long, default_value_t = 1024)]
        vnc_width: u32,
        #[arg(long, default_value_t = 768)]
        vnc_height: u32,
        #[arg(long)]
        tpm: bool,
        #[arg(long, default_value_t = 1)]
        cpus: u32,
        #[arg(long, default_value = "1G")]
        memory: String,
        #[arg(long, requires = "cloud_init")]
        attach: bool,
    },
}

#[derive(Subcommand)]
enum BaseResource {
    Jail {
        name: String,
        #[arg(long)]
        set: Option<String>,
        #[arg(long)]
        version: Option<String>,
    },
    Vm {
        name: String,
    },
}

#[derive(Subcommand)]
enum DestroyResource {
    Dataset {
        name: String,
    },
    Volume {
        name: String,
    },
    Base {
        #[command(subcommand)]
        resource: BaseDestroyResource,
    },
    Jail {
        name: Option<String>,
    },
    Vm {
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum BaseDestroyResource {
    Jail { name: String },
    Vm { name: String },
}

#[derive(Clone, ValueEnum)]
enum DependencyResource {
    Jail,
    Vm,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let direct_command = matches!(&cli.command, Commands::Init { .. });
    if std::env::var_os("PRIOSUN_DAEMON_EXEC").is_none() && !direct_command {
        let args: Vec<String> = if matches!(&cli.command, Commands::Up) {
            service::up_args()?
        } else if matches!(&cli.command, Commands::Down) {
            service::down_args()?
        } else if matches!(&cli.command, Commands::Provision) {
            service::provision_args()?
        } else if matches!(&cli.command, Commands::Attach { name: None }) {
            service::attach_args()?
        } else if matches!(
            &cli.command,
            Commands::Destroy {
                resource: Some(
                    DestroyResource::Jail { name: None } | DestroyResource::Vm { name: None },
                )
            }
        ) || matches!(&cli.command, Commands::Destroy { resource: None })
        {
            service::destroy_args()?
        } else {
            std::env::args().skip(1).collect()
        };
        if args == ["list", "all"] {
            for resource in ["datasets", "volumes", "jails", "vms"] {
                println!("=== {resource} ===");
                let request = vec!["list".to_string(), resource.to_string()];
                let envelope = protocol::send_request(&request)?;
                let status = print_response(&envelope)?;
                if status != 0 {
                    std::process::exit(status);
                }
            }
            std::process::exit(0);
        }
        let envelope = protocol::send_request(&args)?;
        std::process::exit(print_response(&envelope)?);
    }

    match cli.command {
        Commands::Create { resource } => match resource {
            CreateResource::Dataset { name } => {
                let config = config::Config::load()?;
                let dataset = zfs_name(&config.zfs_pool, &name);
                let (_, transcript) = util::cmd::capture(|| {
                    util::cmd::run("zfs", &["create", "-p", &dataset]).map(|_| ())
                })?;
                print_transcript(&transcript)?;
            }
            CreateResource::Volume { name, size } => {
                let config = config::Config::load()?;
                let volume = zfs_name(&config.zfs_pool, &name);
                let (_, transcript) = util::cmd::capture(|| {
                    util::cmd::run("zfs", &["create", "-p", "-V", &size, &volume]).map(|_| ())
                })?;
                print_transcript(&transcript)?;
            }
            CreateResource::Jail {
                name,
                set,
                base,
                version,
                ssh_key,
                start,
                attach,
            } => {
                let config = config::Config::load()?;
                let (_, transcript) = util::cmd::capture(|| {
                    jail::create(
                        &name,
                        set.as_deref(),
                        version.as_deref(),
                        false,
                        base.as_deref(),
                        ssh_key.as_deref(),
                        &config,
                    )?;
                    if start {
                        jail::start(&name, &config)?;
                    }
                    if attach {
                        bail!("create --attach is handled by priosund");
                    }
                    Ok(())
                })?;
                print_transcript(&transcript)?;
            }
            CreateResource::Base { resource } => match resource {
                BaseResource::Jail { name, set, version } => {
                    let config = config::Config::load()?;
                    let (_, transcript) = util::cmd::capture(|| {
                        jail::create(
                            &name,
                            set.as_deref(),
                            version.as_deref(),
                            true,
                            None,
                            None,
                            &config,
                        )
                    })?;
                    print_transcript(&transcript)?;
                }
                BaseResource::Vm { .. } => {
                    bail!("base VM creation is not supported yet")
                }
            },
            CreateResource::Vm {
                name,
                disk,
                os,
                version,
                cloud_init,
                ssh_key,
                iso,
                vnc_port,
                vnc_bind,
                vnc_width,
                vnc_height,
                tpm,
                cpus,
                memory,
                attach,
            } => {
                let config = config::Config::load()?;
                bhyve::create(
                    &name,
                    &bhyve::VmCreateOptions {
                        disk: &disk,
                        os: &os,
                        version: version.as_deref(),
                        cloud_init,
                        ssh_key: ssh_key.as_deref(),
                        iso: iso.as_deref(),
                        vnc_port,
                        vnc_bind: &vnc_bind,
                        vnc_width,
                        vnc_height,
                        tpm,
                        cpus,
                        memory: &memory,
                    },
                    &config,
                )?;
                if cloud_init {
                    bhyve::start_with_seed(&name, &config)?;
                }
                if attach {
                    bail!("create --attach is handled by priosund");
                }
            }
        },
        Commands::Destroy {
            resource: Some(resource),
        } => {
            let config = config::Config::load()?;
            match resource {
                DestroyResource::Vm { name } => {
                    let name = name.ok_or_else(|| anyhow!("destroy requires a service name"))?;
                    bhyve::destroy(&name, &config)?;
                }
                DestroyResource::Jail { name } => {
                    let name = name.ok_or_else(|| anyhow!("destroy requires a service name"))?;
                    jail::destroy(&name, &config)?;
                }
                DestroyResource::Base { resource } => match resource {
                    BaseDestroyResource::Jail { name } => jail::destroy_base(&name, &config)?,
                    BaseDestroyResource::Vm { .. } => {
                        bail!("base VM destruction is not supported yet")
                    }
                },
                DestroyResource::Dataset { name } | DestroyResource::Volume { name } => {
                    let zfs_name = zfs_name(&config.zfs_pool, &name);
                    util::cmd::run("zfs", &["destroy", "-f", &zfs_name])?;
                }
            }
        }
        Commands::Destroy { resource: None } => {
            bail!("destroy requires a resource or an initialized service directory");
        }
        Commands::Start { name, attach } => {
            if attach {
                bail!("start --attach is handled by priosund");
            }
            let config = config::Config::load()?;
            if bhyve::path_exists(&name, &config) {
                bhyve::start(&name, &config)?;
            } else if jail::path_exists(&name, &config) {
                jail::start(&name, &config)?;
            } else {
                bail!("no VM or jail exists with this name: {}", name);
            }
        }
        Commands::Stop { name } => {
            let config = config::Config::load()?;
            if bhyve::path_exists(&name, &config) {
                bhyve::stop(&name, &config)?;
            } else if jail::path_exists(&name, &config) {
                jail::stop(&name, &config)?;
            } else {
                bail!("no VM or jail exists with this name: {}", name);
            }
        }
        Commands::Attach { name: _ } => {
            bail!("attach is handled by priosund");
        }
        Commands::Dependencies {
            resource,
            name,
            dependencies,
        } => {
            let values = dependencies
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect();
            let config = config::Config::load()?;
            match resource {
                DependencyResource::Jail => jail::set_dependencies(&name, values, &config)?,
                DependencyResource::Vm => bhyve::set_dependencies(&name, values, &config)?,
            }
        }
        Commands::Enable { name } => {
            let config = config::Config::load()?;
            set_enabled(&name, true, &config)?;
        }
        Commands::Disable { name } => {
            let config = config::Config::load()?;
            set_enabled(&name, false, &config)?;
        }
        Commands::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
        }
        Commands::NetworkInit => {
            let config = config::Config::load()?;
            network::init(&config)?;
        }
        Commands::Up => {
            bail!("up must be run without daemon execution");
        }
        Commands::Down => {
            bail!("down must be run without daemon execution");
        }
        Commands::Provision => {
            bail!("provision must be run without daemon execution");
        }
        Commands::Init {
            provisioner,
            container,
            develop,
            name,
        } => {
            let provisioner = provisioner.map(|value| match value {
                Provisioner::Ansible => "ansible",
            });
            let container = match container {
                Container::Jail => "jail",
                Container::Vm => "vm",
            };
            service::init(&name, container, provisioner, develop)?;
        }
        Commands::List { resource } => match resource {
            ListResource::Datasets => {
                print_table(zfs_rows("datasets", &["list", "-H", "-o", "name"])?);
            }
            ListResource::Volumes => {
                print_table(zfs_rows(
                    "volumes",
                    &["list", "-H", "-o", "name", "-t", "volume"],
                )?);
            }
            ListResource::Jails => {
                print_table(jail_rows()?);
            }
            ListResource::Vms => {
                let config = config::Config::load()?;
                print_table(vm_rows(&config)?);
            }
            ListResource::All => {
                let config = config::Config::load()?;
                let mut rows = zfs_rows("datasets", &["list", "-H", "-o", "name"])?;
                rows.extend(zfs_rows(
                    "volumes",
                    &["list", "-H", "-o", "name", "-t", "volume"],
                )?);
                rows.extend(jail_rows()?);
                rows.extend(vm_rows(&config)?);
                print_table(rows);
            }
        },
    }

    Ok(())
}

fn zfs_rows(kind: &str, args: &[&str]) -> Result<Vec<ResourceRow>> {
    let output = util::cmd::run("zfs", args)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|name| !name.trim().is_empty())
        .map(|name| ResourceRow {
            kind: kind.to_string(),
            name: name.trim().to_string(),
            status: String::new(),
        })
        .collect())
}

fn print_transcript(transcript: &util::cmd::Transcript) -> Result<()> {
    std::io::stdout().write_all(&transcript.stdout)?;
    std::io::stderr().write_all(&transcript.stderr)?;
    Ok(())
}

fn jail_rows() -> Result<Vec<ResourceRow>> {
    let config = config::Config::load()?;
    Ok(jail::list(&config)?
        .into_iter()
        .map(|jail| ResourceRow {
            kind: "jail".to_string(),
            name: jail.name,
            status: format!("{} {} ({})", jail.hostname, jail.ips, jail.status),
        })
        .collect())
}

fn vm_rows(config: &config::Config) -> Result<Vec<ResourceRow>> {
    Ok(bhyve::list(config)?
        .into_iter()
        .map(|(name, running)| ResourceRow {
            kind: "vm".to_string(),
            name,
            status: if running {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
        })
        .collect())
}

fn print_table(rows: Vec<ResourceRow>) {
    println!("{}", tabled::Table::new(rows));
}

fn print_response(envelope: &nvtree::Nvtree) -> Result<i32> {
    if let Some(Nvtvalue::String(error)) = nvtree_find(envelope, "error").map(|pair| &pair.value) {
        eprintln!("{error}");
        return Ok(1);
    }
    let response = match nvtree_find(envelope, "response").map(|pair| &pair.value) {
        Some(Nvtvalue::Nested(response)) => response,
        _ => bail!("daemon response is missing response data"),
    };
    let status = match nvtree_find(response, "status").map(|pair| &pair.value) {
        Some(Nvtvalue::Number(status)) => *status as i32,
        _ => 0,
    };
    if ["datasets", "volumes", "jails", "vms"]
        .iter()
        .any(|resource| nvtree_find(response, resource).is_some())
    {
        print!("{}", output::nvtree_to_table(response)?);
    } else {
        use std::io::Write;
        if let Some(Nvtvalue::String(stdout)) =
            nvtree_find(response, "stdout").map(|pair| &pair.value)
        {
            std::io::stdout().write_all(stdout.as_bytes())?;
        }
        if let Some(Nvtvalue::String(stderr)) =
            nvtree_find(response, "stderr").map(|pair| &pair.value)
        {
            std::io::stderr().write_all(stderr.as_bytes())?;
        }
    }
    Ok(status)
}

fn zfs_name(pool: &str, name: &str) -> String {
    if name == pool || name.starts_with(&format!("{pool}/")) {
        name.to_string()
    } else {
        format!("{pool}/{name}")
    }
}

fn set_enabled(name: &str, enabled: bool, config: &config::Config) -> Result<()> {
    if jail::path_exists(name, config) {
        jail::set_enabled(name, enabled, config)?;
    } else if bhyve::path_exists(name, config) {
        bhyve::set_enabled(name, enabled, config)?;
    } else {
        bail!("no VM or jail exists with this name: {name}");
    }
    Ok(())
}
