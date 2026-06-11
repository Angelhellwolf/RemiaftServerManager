use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ConfigStore {
    config_path: PathBuf,
    runtime_dir: PathBuf,
}

impl ConfigStore {
    pub fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow!("cannot resolve user config directory"))?
            .join("remiaft");
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| config_dir.clone())
            .join("remiaft");

        fs::create_dir_all(&config_dir)
            .with_context(|| format!("create config dir {}", config_dir.display()))?;
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("create runtime dir {}", data_dir.display()))?;

        Ok(Self {
            config_path: config_dir.join("config.toml"),
            runtime_dir: data_dir.join("runtime"),
        })
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn load(&self) -> Result<RemiaftConfig> {
        fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("create runtime dir {}", self.runtime_dir.display()))?;
        if !self.config_path.exists() {
            let config = RemiaftConfig::default();
            self.save(&config)?;
            return Ok(config);
        }

        let raw = fs::read_to_string(&self.config_path)
            .with_context(|| format!("read {}", self.config_path.display()))?;
        let mut config: RemiaftConfig = toml::from_str(&raw)
            .with_context(|| format!("parse {}", self.config_path.display()))?;
        if config.normalize_startup_commands() {
            self.save(&config)?;
        }
        Ok(config)
    }

    pub fn save(&self, config: &RemiaftConfig) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(config)?;
        fs::write(&self.config_path, raw)
            .with_context(|| format!("write {}", self.config_path.display()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemiaftConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub java_path: String,
    #[serde(default)]
    pub groups: Vec<ServerGroup>,
    pub servers: Vec<ServerConfig>,
}

impl Default for RemiaftConfig {
    fn default() -> Self {
        Self {
            language: None,
            java_path: "java".to_string(),
            groups: Vec::new(),
            servers: Vec::new(),
        }
    }
}

impl RemiaftConfig {
    pub fn add_server(&mut self, name: String, directory: PathBuf, jar_path: PathBuf) {
        let id = format!("{}-{}", slug(&name), &Uuid::new_v4().to_string()[..8]);
        self.servers.push(ServerConfig {
            id,
            name,
            group_id: None,
            directory,
            startup_command: None,
            jar_path,
            java_path: None,
            min_memory_mb: 1024,
            max_memory_mb: 4096,
            java_args: Vec::new(),
            server_args: vec!["nogui".to_string()],
            auto_restart: false,
            restart_delay_secs: 10,
            version: None,
            runtime: ServerRuntimeConfig::default(),
        });
    }

    pub fn find_server(&self, key: &str) -> Result<&ServerConfig> {
        self.servers
            .iter()
            .find(|server| server.id == key || server.name == key)
            .ok_or_else(|| anyhow!("unknown server: {key}"))
    }

    pub fn find_server_index(&self, key: &str) -> Result<usize> {
        self.servers
            .iter()
            .position(|server| server.id == key || server.name == key)
            .ok_or_else(|| anyhow!("unknown server: {key}"))
    }

    pub fn ensure_group_path(&mut self, path: &str) -> Option<String> {
        let mut parent_id: Option<String> = None;
        let mut last_id = None;
        for name in path
            .split('/')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let existing = self
                .groups
                .iter()
                .find(|group| group.parent_id == parent_id && group.name == name)
                .map(|group| group.id.clone());
            let id = if let Some(id) = existing {
                id
            } else {
                let id = format!("{}-{}", slug(name), &Uuid::new_v4().to_string()[..8]);
                self.groups.push(ServerGroup {
                    id: id.clone(),
                    name: name.to_string(),
                    parent_id: parent_id.clone(),
                });
                id
            };
            parent_id = Some(id.clone());
            last_id = Some(id);
        }
        last_id
    }

    pub fn delete_group_preserving_servers(&mut self, group_id: &str) -> Option<DeletedGroup> {
        let group = self
            .groups
            .iter()
            .find(|group| group.id == group_id)
            .cloned()?;
        let mut removed_group_ids = vec![group.id.clone()];
        self.collect_descendant_group_ids(group_id, &mut removed_group_ids);
        let removed_groups = removed_group_ids.iter().cloned().collect::<HashSet<_>>();

        let mut moved_server_count = 0;
        for server in &mut self.servers {
            if server
                .group_id
                .as_ref()
                .is_some_and(|id| removed_groups.contains(id))
            {
                server.group_id = group.parent_id.clone();
                moved_server_count += 1;
            }
        }

        self.groups
            .retain(|group| !removed_groups.contains(&group.id));

        Some(DeletedGroup {
            name: group.name,
            removed_group_ids,
            moved_server_count,
        })
    }

    fn collect_descendant_group_ids(&self, group_id: &str, output: &mut Vec<String>) {
        let child_ids = self
            .groups
            .iter()
            .filter(|group| group.parent_id.as_deref() == Some(group_id))
            .map(|group| group.id.clone())
            .collect::<Vec<_>>();
        for child_id in child_ids {
            output.push(child_id.clone());
            self.collect_descendant_group_ids(&child_id, output);
        }
    }

    fn normalize_startup_commands(&mut self) -> bool {
        let mut changed = false;
        for server in &mut self.servers {
            if let Some(jar_index) = server.java_args.iter().position(|part| part == "-jar") {
                if let Some(first) = server.java_args.first() {
                    if looks_like_java_bin(first) {
                        server.java_path = Some(first.clone());
                    }
                }
                if let Some(jar_path) = server.java_args.get(jar_index + 1) {
                    server.jar_path = PathBuf::from(jar_path);
                }
                server.server_args = server
                    .java_args
                    .get(jar_index + 2..)
                    .unwrap_or_default()
                    .to_vec();
                let java_arg_start = usize::from(
                    server
                        .java_args
                        .first()
                        .map(|part| looks_like_java_bin(part))
                        .unwrap_or(false),
                );
                server.java_args = server.java_args[java_arg_start..jar_index].to_vec();
                server.startup_command = Some(server.startup_command(&self.java_path));
                changed = true;
                continue;
            }

            if let Some(startup_command) = server
                .startup_command
                .as_deref()
                .map(str::trim)
                .filter(|command| !command.is_empty())
            {
                let legacy_java_args = server.java_args.join(" ");
                if !server.java_args.is_empty()
                    && server.server_args.is_empty()
                    && legacy_java_args == startup_command
                {
                    server.java_args.clear();
                    changed = true;
                }
            }
        }
        changed
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeletedGroup {
    pub name: String,
    pub removed_group_ids: Vec<String>,
    pub moved_server_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerGroup {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub directory: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_command: Option<String>,
    pub jar_path: PathBuf,
    pub java_path: Option<String>,
    pub min_memory_mb: u32,
    pub max_memory_mb: u32,
    pub java_args: Vec<String>,
    pub server_args: Vec<String>,
    pub auto_restart: bool,
    pub restart_delay_secs: u64,
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub runtime: ServerRuntimeConfig,
}

impl ServerConfig {
    pub fn java_bin<'a>(&'a self, default: &'a str) -> &'a str {
        self.java_path.as_deref().unwrap_or(default)
    }

    pub fn startup_command(&self, default_java: &str) -> String {
        let mut parts = vec![
            self.java_bin(default_java).to_string(),
            format!("-Xms{}M", self.min_memory_mb),
            format!("-Xmx{}M", self.max_memory_mb),
        ];
        parts.extend(self.java_args.clone());
        parts.push("-jar".to_string());
        parts.push(self.jar_path.to_string_lossy().to_string());
        parts.extend(self.server_args.clone());
        parts.join(" ")
    }

    pub fn uses_docker(&self) -> bool {
        self.runtime.kind == ServerRuntimeKind::Docker
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ServerRuntimeConfig {
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: ServerRuntimeKind,
    #[serde(default, skip_serializing_if = "is_default")]
    pub docker: DockerServerConfig,
}

impl Default for ServerRuntimeConfig {
    fn default() -> Self {
        Self {
            kind: ServerRuntimeKind::Native,
            docker: DockerServerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ServerRuntimeKind {
    #[default]
    Native,
    Docker,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct DockerServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_candidates: Vec<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub image_policy: DockerImagePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<DockerPortMapping>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<DockerVolumeMount>,
    #[serde(default = "default_mount_server_directory")]
    pub mount_server_directory: bool,
    #[serde(default = "default_docker_server_dir")]
    pub server_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_command: Option<String>,
    #[serde(default)]
    pub use_image_entrypoint: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub user: DockerUserConfig,
    #[serde(default, skip_serializing_if = "is_default")]
    pub rcon: RconConfig,
    #[serde(default)]
    pub auto_remove: bool,
}

impl Default for DockerServerConfig {
    fn default() -> Self {
        Self {
            image: None,
            image_candidates: Vec::new(),
            image_policy: DockerImagePolicy::IfMissing,
            container_name: None,
            network: None,
            labels: BTreeMap::new(),
            environment: BTreeMap::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            mount_server_directory: true,
            server_dir: default_docker_server_dir(),
            working_dir: None,
            startup_command: None,
            use_image_entrypoint: false,
            user: DockerUserConfig::default(),
            rcon: RconConfig::default(),
            auto_remove: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum DockerImagePolicy {
    #[default]
    IfMissing,
    Always,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct DockerPortMapping {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub container_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub protocol: DockerProtocol,
}

impl DockerPortMapping {
    pub fn tcp(name: impl Into<String>, container_port: u16, host_port: Option<u16>) -> Self {
        Self {
            name: Some(name.into()),
            container_port,
            host_port,
            host_ip: None,
            protocol: DockerProtocol::Tcp,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum DockerProtocol {
    #[default]
    Tcp,
    Udp,
}

impl DockerProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct DockerVolumeMount {
    pub host: PathBuf,
    pub container: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct DockerUserConfig {
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: DockerUserMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
}

impl Default for DockerUserConfig {
    fn default() -> Self {
        Self {
            mode: DockerUserMode::Auto,
            uid: None,
            gid: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum DockerUserMode {
    #[default]
    Auto,
    Host,
    Image,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct RconConfig {
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: RconMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default = "default_rcon_container_port")]
    pub container_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    #[serde(default = "default_rcon_host")]
    pub host: String,
    #[serde(default = "default_rcon_port_start")]
    pub port_range_start: u16,
    #[serde(default = "default_rcon_port_end")]
    pub port_range_end: u16,
}

impl Default for RconConfig {
    fn default() -> Self {
        Self {
            mode: RconMode::Auto,
            password: None,
            container_port: default_rcon_container_port(),
            host_port: None,
            host: default_rcon_host(),
            port_range_start: default_rcon_port_start(),
            port_range_end: default_rcon_port_end(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum RconMode {
    #[default]
    Auto,
    Manual,
    Disabled,
}

fn slug(input: &str) -> String {
    let slug: String = input
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_ascii_whitespace() || ch == '-' || ch == '_' {
                Some('-')
            } else {
                None
            }
        })
        .collect();

    let cleaned = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if cleaned.is_empty() {
        "server".to_string()
    } else {
        cleaned
    }
}

fn default_docker_server_dir() -> String {
    "/home/remiaft/server".to_string()
}

fn default_mount_server_directory() -> bool {
    true
}

fn default_rcon_container_port() -> u16 {
    25575
}

fn default_rcon_host() -> String {
    "127.0.0.1".to_string()
}

fn default_rcon_port_start() -> u16 {
    25575
}

fn default_rcon_port_end() -> u16 {
    25999
}

fn is_default<T>(value: &T) -> bool
where
    T: Default + PartialEq,
{
    value == &T::default()
}

fn looks_like_java_bin(value: &str) -> bool {
    let name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value);
    name == "java" || name.starts_with("java")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_has_stable_fallback() {
        assert_eq!(slug("Survival SMP"), "survival-smp");
        assert_eq!(slug("***"), "server");
    }

    #[test]
    fn missing_runtime_defaults_to_native() {
        let raw = r#"
java_path = "java"
groups = []

[[servers]]
id = "survival-12345678"
name = "survival"
directory = "."
jar_path = "server.jar"
min_memory_mb = 1024
max_memory_mb = 4096
java_args = []
server_args = ["nogui"]
auto_restart = false
restart_delay_secs = 10
"#;

        let config: RemiaftConfig = toml::from_str(raw).unwrap();

        assert_eq!(config.servers[0].runtime.kind, ServerRuntimeKind::Native);
    }

    #[test]
    fn docker_runtime_config_round_trips() {
        let raw = r#"
java_path = "java"
groups = []

[[servers]]
id = "room-12345678"
name = "room"
directory = "."
jar_path = "server.jar"
min_memory_mb = 1024
max_memory_mb = 4096
java_args = []
server_args = ["nogui"]
auto_restart = false
restart_delay_secs = 10

[servers.runtime]
kind = "docker"

[servers.runtime.docker]
image = "example/room:latest"
mount_server_directory = false
use_image_entrypoint = true
auto_remove = true

[servers.runtime.docker.rcon]
mode = "auto"
host_port = 25575
password = "secret"
"#;

        let config: RemiaftConfig = toml::from_str(raw).unwrap();
        let server = &config.servers[0];

        assert_eq!(server.runtime.kind, ServerRuntimeKind::Docker);
        assert_eq!(
            server.runtime.docker.image.as_deref(),
            Some("example/room:latest")
        );
        assert!(!server.runtime.docker.mount_server_directory);
        assert!(server.runtime.docker.use_image_entrypoint);
        assert_eq!(server.runtime.docker.rcon.host_port, Some(25575));
        assert_eq!(
            server.runtime.docker.rcon.password.as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn deleting_group_preserves_servers_under_parent() {
        let mut config = RemiaftConfig {
            groups: vec![
                ServerGroup {
                    id: "root".to_string(),
                    name: "root".to_string(),
                    parent_id: None,
                },
                ServerGroup {
                    id: "child".to_string(),
                    name: "child".to_string(),
                    parent_id: Some("root".to_string()),
                },
                ServerGroup {
                    id: "sibling".to_string(),
                    name: "sibling".to_string(),
                    parent_id: None,
                },
            ],
            ..Default::default()
        };
        config.add_server(
            "root server".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        config.add_server(
            "child server".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        config.add_server(
            "sibling server".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        config.servers[0].group_id = Some("root".to_string());
        config.servers[1].group_id = Some("child".to_string());
        config.servers[2].group_id = Some("sibling".to_string());

        let deleted = config.delete_group_preserving_servers("root").unwrap();

        assert_eq!(deleted.name, "root");
        assert_eq!(deleted.removed_group_ids, vec!["root", "child"]);
        assert_eq!(deleted.moved_server_count, 2);
        assert_eq!(
            config
                .groups
                .iter()
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>(),
            vec!["sibling"]
        );
        assert_eq!(config.servers[0].group_id, None);
        assert_eq!(config.servers[1].group_id, None);
        assert_eq!(config.servers[2].group_id.as_deref(), Some("sibling"));
    }
}
