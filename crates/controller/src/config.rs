use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiConfig {
    pub product_name: String,
}

impl AppConfig {
    pub async fn load(path: &Path) -> Result<Self> {
        let contents = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read config {}", path.display()))?;

        let mut config: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse config {}", path.display()))?;

        if config.storage.data_dir.is_relative() {
            let base = path.parent().unwrap_or_else(|| Path::new("."));
            config.storage.data_dir = base.join(&config.storage.data_dir);
        }

        Ok(config)
    }

    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.server.host, self.server.port)
    }
}

fn default_host() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

fn default_port() -> u16 {
    8124
}
