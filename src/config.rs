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
        toml::from_str(&raw).with_context(|| format!("parse {}", self.config_path.display()))
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
    pub servers: Vec<ServerConfig>,
}

impl Default for RemiaftConfig {
    fn default() -> Self {
        Self {
            language: None,
            java_path: "java".to_string(),
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
            directory,
            jar_path,
            java_path: None,
            min_memory_mb: 1024,
            max_memory_mb: 4096,
            java_args: Vec::new(),
            server_args: vec!["nogui".to_string()],
            auto_restart: false,
            restart_delay_secs: 10,
            version: None,
        });
    }

    pub fn find_server(&self, key: &str) -> Result<&ServerConfig> {
        self.servers
            .iter()
            .find(|server| server.id == key || server.name == key)
            .ok_or_else(|| anyhow!("unknown server: {key}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    pub directory: PathBuf,
    pub jar_path: PathBuf,
    pub java_path: Option<String>,
    pub min_memory_mb: u32,
    pub max_memory_mb: u32,
    pub java_args: Vec<String>,
    pub server_args: Vec<String>,
    pub auto_restart: bool,
    pub restart_delay_secs: u64,
    pub version: Option<String>,
}

impl ServerConfig {
    pub fn java_bin<'a>(&'a self, default: &'a str) -> &'a str {
        self.java_path.as_deref().unwrap_or(default)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_has_stable_fallback() {
        assert_eq!(slug("Survival SMP"), "survival-smp");
        assert_eq!(slug("***"), "server");
    }
}
