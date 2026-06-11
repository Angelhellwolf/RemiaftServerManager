mod config;
mod docker;
mod docker_api;
mod i18n;
mod manifest;
mod process;
mod rcon;
mod tui;

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::config::{
    ConfigStore, DockerPortMapping, DockerProtocol, DockerVolumeMount, RconMode, ServerConfig,
    ServerRuntimeKind,
};

#[derive(Debug, Parser)]
#[command(
    name = "remiaft",
    version,
    about = "Minecraft server manager",
    disable_version_flag = true
)]
struct Cli {
    #[arg(short = 'v', long = "version", action = ArgAction::Version, help = "Print version")]
    version: Option<bool>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Open the interactive terminal UI.
    Tui,
    /// Add a server entry without opening the TUI.
    Add {
        /// Display name for the server.
        name: String,
        /// Host server directory.
        #[arg(long)]
        dir: PathBuf,
        /// Jar path relative to dir or an absolute path.
        #[arg(long, default_value = "server.jar")]
        jar: PathBuf,
        /// Full startup command to keep with this server.
        #[arg(long)]
        startup: Option<String>,
        /// Optional group path, for example minigames/bedwars.
        #[arg(long)]
        group: Option<String>,
        #[command(flatten)]
        docker: DockerAddArgs,
    },
    /// Configure Docker settings for an existing server.
    Docker {
        /// Server name or id.
        server: String,
        /// Switch the server to the Docker backend.
        #[arg(long)]
        enable: bool,
        /// Switch the server back to the native backend.
        #[arg(long)]
        disable: bool,
        #[command(flatten)]
        options: DockerEditArgs,
    },
    /// Start a configured server by name or id.
    Start { server: String },
    /// Stop a configured server by name or id.
    Stop { server: String },
    /// Restart a configured server by name or id.
    Restart { server: String },
    /// Print configured servers and runtime state.
    Status,
    /// Fetch recent vanilla Minecraft versions from Mojang metadata.
    Versions {
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Internal process supervisor. Do not call directly.
    #[command(hide = true)]
    Supervise { server_id: String },
}

#[derive(Debug, Args)]
struct DockerAddArgs {
    /// Create this server as a Docker-backed server.
    #[arg(long)]
    docker: bool,
    /// Docker image. Empty Docker image config uses Remiaft's Java image default.
    #[arg(long)]
    image: Option<String>,
    /// Do not bind-mount the server directory into the container.
    #[arg(long)]
    no_mount: bool,
    /// Use the image ENTRYPOINT/CMD instead of Remiaft's Java command.
    #[arg(long)]
    use_image_entrypoint: bool,
    /// Docker-specific startup command.
    #[arg(long)]
    docker_startup: Option<String>,
    /// Docker network name.
    #[arg(long)]
    network: Option<String>,
    /// RCON mode.
    #[arg(long, value_enum)]
    rcon: Option<CliRconMode>,
    /// Fixed host RCON port.
    #[arg(long)]
    rcon_host_port: Option<u16>,
    /// Start of the RCON auto-allocation range.
    #[arg(long)]
    rcon_port_start: Option<u16>,
    /// End of the RCON auto-allocation range.
    #[arg(long)]
    rcon_port_end: Option<u16>,
    /// Port mapping: container, host:container, or ip:host:container with optional /tcp or /udp.
    #[arg(long)]
    port: Vec<String>,
    /// Volume mapping: host:container or host:container:ro.
    #[arg(long)]
    volume: Vec<String>,
    /// Ask Docker to remove the container after it exits.
    #[arg(long)]
    auto_remove: bool,
}

#[derive(Debug, Args)]
struct DockerEditArgs {
    /// Docker image.
    #[arg(long)]
    image: Option<String>,
    /// Docker network name.
    #[arg(long)]
    network: Option<String>,
    /// Bind-mount the server directory into the container.
    #[arg(long)]
    mount: bool,
    /// Do not bind-mount the server directory into the container.
    #[arg(long)]
    no_mount: bool,
    /// Use the image ENTRYPOINT/CMD instead of Remiaft's Java command.
    #[arg(long)]
    use_image_entrypoint: bool,
    /// Use Remiaft's generated/container startup command instead of image entrypoint.
    #[arg(long)]
    no_image_entrypoint: bool,
    /// Docker-specific startup command.
    #[arg(long)]
    docker_startup: Option<String>,
    /// RCON mode.
    #[arg(long, value_enum)]
    rcon: Option<CliRconMode>,
    /// Fixed host RCON port.
    #[arg(long)]
    rcon_host_port: Option<u16>,
    /// Start of the RCON auto-allocation range.
    #[arg(long)]
    rcon_port_start: Option<u16>,
    /// End of the RCON auto-allocation range.
    #[arg(long)]
    rcon_port_end: Option<u16>,
    /// Replace existing port mappings with none before applying --port.
    #[arg(long)]
    clear_ports: bool,
    /// Add a port mapping: container, host:container, or ip:host:container with optional /tcp or /udp.
    #[arg(long)]
    port: Vec<String>,
    /// Replace existing volumes with none before applying --volume.
    #[arg(long)]
    clear_volumes: bool,
    /// Add a volume mapping: host:container or host:container:ro.
    #[arg(long)]
    volume: Vec<String>,
    /// Set Docker AutoRemove=true.
    #[arg(long)]
    auto_remove: bool,
    /// Set Docker AutoRemove=false.
    #[arg(long)]
    no_auto_remove: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliRconMode {
    Auto,
    Manual,
    Disabled,
}

impl From<CliRconMode> for RconMode {
    fn from(value: CliRconMode) -> Self {
        match value {
            CliRconMode::Auto => Self::Auto,
            CliRconMode::Manual => Self::Manual,
            CliRconMode::Disabled => Self::Disabled,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = ConfigStore::new()?;

    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Tui => tui::run(store).await,
        Commands::Add {
            name,
            dir,
            jar,
            startup,
            group,
            docker,
        } => {
            let mut config = store.load()?;
            let group_id = group
                .as_deref()
                .and_then(|path| config.ensure_group_path(path));
            config.add_server(name.clone(), dir, jar);
            let server = config
                .servers
                .last_mut()
                .ok_or_else(|| anyhow!("server was not added"))?;
            server.group_id = group_id;
            if let Some(startup) = startup {
                server.startup_command = Some(startup);
            }
            apply_docker_add_args(server, docker)?;
            if server.uses_docker() {
                crate::docker::validate_server_config(server)?;
            }
            store.save(&config)?;
            println!("added {}", name);
            Ok(())
        }
        Commands::Docker {
            server,
            enable,
            disable,
            options,
        } => {
            if enable && disable {
                bail!("--enable and --disable cannot be used together");
            }
            let mut config = store.load()?;
            let index = config.find_server_index(&server)?;
            let server = &mut config.servers[index];
            if disable {
                server.runtime.kind = ServerRuntimeKind::Native;
            }
            if enable || options.has_any() {
                server.runtime.kind = ServerRuntimeKind::Docker;
            }
            apply_docker_edit_args(server, options)?;
            if server.uses_docker() {
                crate::docker::validate_server_config(server)?;
            }
            let name = server.name.clone();
            store.save(&config)?;
            println!("configured {}", name);
            Ok(())
        }
        Commands::Start { server } => {
            let mut config = store.load()?;
            process::start_server(&store, &mut config, &server)?;
            let server = config.find_server(&server)?;
            println!("started {}", server.name);
            Ok(())
        }
        Commands::Stop { server } => {
            let config = store.load()?;
            let server = config.find_server(&server)?;
            process::stop_server(&store, server)?;
            println!("stopped {}", server.name);
            Ok(())
        }
        Commands::Restart { server } => {
            let mut config = store.load()?;
            let server = config.find_server(&server)?;
            let name = server.name.clone();
            let id = server.id.clone();
            process::restart_server(&store, &mut config, &id)?;
            println!("restarted {}", name);
            Ok(())
        }
        Commands::Status => {
            let config = store.load()?;
            for server in &config.servers {
                let status = process::runtime_status(&store, server);
                println!(
                    "{:<20} {:<10} {}",
                    server.name,
                    status.label(),
                    server.directory.display()
                );
            }
            Ok(())
        }
        Commands::Versions { limit } => {
            let versions = manifest::fetch_versions(limit).await?;
            for version in versions {
                println!(
                    "{} {} {}",
                    version.id,
                    version.kind,
                    version.server_url.unwrap_or_default()
                );
            }
            Ok(())
        }
        Commands::Supervise { server_id } => process::run_supervisor(&store, &server_id),
    }
}

fn apply_docker_add_args(server: &mut ServerConfig, args: DockerAddArgs) -> Result<()> {
    let enable_docker = args.docker
        || args.image.is_some()
        || args.no_mount
        || args.use_image_entrypoint
        || args.docker_startup.is_some()
        || args.network.is_some()
        || args.rcon.is_some()
        || args.rcon_host_port.is_some()
        || args.rcon_port_start.is_some()
        || args.rcon_port_end.is_some()
        || !args.port.is_empty()
        || !args.volume.is_empty()
        || args.auto_remove;
    if !enable_docker {
        return Ok(());
    }

    server.runtime.kind = ServerRuntimeKind::Docker;
    let docker = &mut server.runtime.docker;
    docker.image = args.image;
    docker.network = args.network;
    docker.mount_server_directory = !args.no_mount;
    docker.use_image_entrypoint = args.use_image_entrypoint;
    docker.startup_command = args.docker_startup;
    docker.auto_remove = args.auto_remove;
    if let Some(mode) = args.rcon {
        docker.rcon.mode = mode.into();
    }
    if let Some(port) = args.rcon_host_port {
        docker.rcon.host_port = Some(port);
    }
    if let Some(start) = args.rcon_port_start {
        docker.rcon.port_range_start = start;
    }
    if let Some(end) = args.rcon_port_end {
        docker.rcon.port_range_end = end;
    }
    for port in args.port {
        docker.ports.push(parse_port_mapping(&port)?);
    }
    for volume in args.volume {
        docker.volumes.push(parse_volume_mount(&volume)?);
    }
    Ok(())
}

fn apply_docker_edit_args(server: &mut ServerConfig, args: DockerEditArgs) -> Result<()> {
    if args.mount && args.no_mount {
        bail!("--mount and --no-mount cannot be used together");
    }
    if args.use_image_entrypoint && args.no_image_entrypoint {
        bail!("--use-image-entrypoint and --no-image-entrypoint cannot be used together");
    }
    if args.auto_remove && args.no_auto_remove {
        bail!("--auto-remove and --no-auto-remove cannot be used together");
    }

    let docker = &mut server.runtime.docker;
    if let Some(image) = args.image {
        docker.image = if image.trim().is_empty() {
            None
        } else {
            Some(image)
        };
    }
    if let Some(network) = args.network {
        docker.network = if network.trim().is_empty() {
            None
        } else {
            Some(network)
        };
    }
    if args.mount {
        docker.mount_server_directory = true;
    }
    if args.no_mount {
        docker.mount_server_directory = false;
    }
    if args.use_image_entrypoint {
        docker.use_image_entrypoint = true;
    }
    if args.no_image_entrypoint {
        docker.use_image_entrypoint = false;
    }
    if let Some(startup) = args.docker_startup {
        docker.startup_command = if startup.trim().is_empty() {
            None
        } else {
            Some(startup)
        };
    }
    if let Some(mode) = args.rcon {
        docker.rcon.mode = mode.into();
    }
    if let Some(port) = args.rcon_host_port {
        docker.rcon.host_port = Some(port);
    }
    if let Some(start) = args.rcon_port_start {
        docker.rcon.port_range_start = start;
    }
    if let Some(end) = args.rcon_port_end {
        docker.rcon.port_range_end = end;
    }
    if args.clear_ports {
        docker.ports.clear();
    }
    for port in args.port {
        docker.ports.push(parse_port_mapping(&port)?);
    }
    if args.clear_volumes {
        docker.volumes.clear();
    }
    for volume in args.volume {
        docker.volumes.push(parse_volume_mount(&volume)?);
    }
    if args.auto_remove {
        docker.auto_remove = true;
    }
    if args.no_auto_remove {
        docker.auto_remove = false;
    }
    Ok(())
}

impl DockerEditArgs {
    fn has_any(&self) -> bool {
        self.image.is_some()
            || self.network.is_some()
            || self.mount
            || self.no_mount
            || self.use_image_entrypoint
            || self.no_image_entrypoint
            || self.docker_startup.is_some()
            || self.rcon.is_some()
            || self.rcon_host_port.is_some()
            || self.rcon_port_start.is_some()
            || self.rcon_port_end.is_some()
            || self.clear_ports
            || !self.port.is_empty()
            || self.clear_volumes
            || !self.volume.is_empty()
            || self.auto_remove
            || self.no_auto_remove
    }
}

fn parse_port_mapping(raw: &str) -> Result<DockerPortMapping> {
    let (port_part, protocol) = match raw.rsplit_once('/') {
        Some((ports, "tcp")) => (ports, DockerProtocol::Tcp),
        Some((ports, "udp")) => (ports, DockerProtocol::Udp),
        Some((_, protocol)) => bail!("unsupported port protocol: {protocol}"),
        None => (raw, DockerProtocol::Tcp),
    };
    let parts = port_part.split(':').collect::<Vec<_>>();
    let (host_ip, host_port, container_port) = match parts.as_slice() {
        [container] => (None, None, parse_u16(container, "container port")?),
        [host, container] => (
            None,
            Some(parse_u16(host, "host port")?),
            parse_u16(container, "container port")?,
        ),
        [ip, host, container] => (
            Some((*ip).to_string()),
            Some(parse_u16(host, "host port")?),
            parse_u16(container, "container port")?,
        ),
        _ => bail!("invalid port mapping: {raw}"),
    };
    Ok(DockerPortMapping {
        name: None,
        container_port,
        host_port,
        host_ip,
        protocol,
    })
}

fn parse_volume_mount(raw: &str) -> Result<DockerVolumeMount> {
    let parts = raw.split(':').collect::<Vec<_>>();
    let (host, container, mode) = match parts.as_slice() {
        [host, container] => (*host, *container, "rw"),
        [host, container, mode] => (*host, *container, *mode),
        _ => bail!("invalid volume mapping: {raw}"),
    };
    let read_only = match mode {
        "rw" => false,
        "ro" => true,
        _ => bail!("invalid volume mode: {mode}"),
    };
    Ok(DockerVolumeMount {
        host: PathBuf::from(host),
        container: container.to_string(),
        read_only,
    })
}

fn parse_u16(value: &str, label: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .map_err(|err| anyhow!("invalid {label} {value}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_port_mappings() {
        let port = parse_port_mapping("127.0.0.1:25575:25575/tcp").unwrap();
        assert_eq!(port.host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(port.host_port, Some(25575));
        assert_eq!(port.container_port, 25575);
        assert_eq!(port.protocol, DockerProtocol::Tcp);

        let port = parse_port_mapping("19132/udp").unwrap();
        assert_eq!(port.host_port, None);
        assert_eq!(port.container_port, 19132);
        assert_eq!(port.protocol, DockerProtocol::Udp);
    }

    #[test]
    fn parses_cli_volume_mounts() {
        let volume = parse_volume_mount("/host/plugins:/server/plugins:ro").unwrap();
        assert_eq!(volume.host, PathBuf::from("/host/plugins"));
        assert_eq!(volume.container, "/server/plugins");
        assert!(volume.read_only);
    }
}
