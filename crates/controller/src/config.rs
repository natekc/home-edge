use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub ui: UiConfig,
    #[serde(default)]
    pub areas: AreasConfig,
    #[serde(default)]
    pub history: HistoryConfig,
}

/// Initial area names used to seed the area registry on first boot.
///
/// After the first boot, areas are managed dynamically through the WS API
/// (`config/area_registry/{create,update,delete}`) and persisted in
/// `<data_dir>/area_registry.json`; this list is ignored from that point on.
///
/// Define in `config.toml`:
/// ```toml
/// [areas]
/// names = ["Living Room", "Kitchen", "Bedroom"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AreasConfig {
    #[serde(default = "default_areas")]
    pub names: Vec<String>,
}

fn default_areas() -> Vec<String> {
    vec![]
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Tracing log level. `RUST_LOG` takes precedence when set.
    #[serde(default = "default_log_level", deserialize_with = "deserialize_level")]
    pub log_level: tracing::Level,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiConfig {
    pub product_name: String,
}

/// History ring-buffer configuration.
///
/// ```toml
/// [history]
/// capacity = 1000   # max readings retained per entity
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConfig {
    /// Maximum number of sensor readings to retain per entity.
    /// Oldest entries are evicted when the buffer is full.
    /// Default: 1000.
    #[serde(default = "default_history_capacity")]
    pub capacity: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { capacity: default_history_capacity() }
    }
}

fn default_history_capacity() -> usize {
    1000
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

fn default_log_level() -> tracing::Level {
    tracing::Level::INFO
}

fn deserialize_level<'de, D: serde::Deserializer<'de>>(d: D) -> Result<tracing::Level, D::Error> {
    let s = String::deserialize(d)?;
    s.parse().map_err(serde::de::Error::custom)
}


