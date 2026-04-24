//! Persistent store for Zigbee devices discovered via the embedded bridge.
//!
//! Follows the same `Mutex + RwLock + JSON file` pattern as
//! [`MobileDeviceStore`](crate::mobile_device_store::MobileDeviceStore).
//!
//! File: `<data_dir>/zigbee_devices.json`

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::storage::save_json_atomic;

/// A single Zigbee device record, persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZigbeeDeviceRecord {
    /// IEEE 802.15.4 extended address, hex-encoded (e.g. `"0xec1bbdfffeaa66db"`).
    /// This is the stable unique key — it never changes across reboots or rejoins.
    pub ieee_addr: String,
    /// User-facing name. Defaults to the IEEE address string until the user
    /// renames the device.
    pub friendly_name: String,
    /// Manufacturer name from ZCL Basic cluster (attribute 0x0004), if known.
    #[serde(default)]
    pub manufacturer: Option<String>,
    /// Model identifier from ZCL Basic cluster (attribute 0x0005), if known.
    #[serde(default)]
    pub model: Option<String>,
    /// Power source reported by the device (`"Mains"`, `"Battery"`, etc.).
    #[serde(default)]
    pub power_source: Option<String>,
    /// Software build ID from ZCL Basic cluster (attribute 0x4000), if known.
    #[serde(default)]
    pub sw_build_id: Option<String>,
    /// True once the device interview (endpoint + cluster negotiation) is done.
    pub interview_complete: bool,
    /// ISO 8601 timestamp of the last received message from this device.
    #[serde(default)]
    pub last_seen: Option<String>,
    /// User-set display name overriding `friendly_name` in the UI.
    #[serde(default)]
    pub name_by_user: Option<String>,
    /// Area/room assigned to this device by the user.
    #[serde(default)]
    pub user_area_id: Option<String>,
}

impl ZigbeeDeviceRecord {
    /// The display name shown in the UI: user override → friendly_name.
    pub fn display_name(&self) -> &str {
        self.name_by_user.as_deref().unwrap_or(&self.friendly_name)
    }
}

/// Partial update applied via the rename / area-assign API.
pub struct ZigbeeDeviceMetaUpdate {
    pub name_by_user: Option<Option<String>>,
    pub user_area_id: Option<Option<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ZigbeeDeviceStoreData {
    devices: Vec<ZigbeeDeviceRecord>,
}

/// Thread-safe, lazily-loaded store for Zigbee device records.
pub struct ZigbeeDeviceStore {
    root: PathBuf,
    lock: Mutex<()>,
    cache: RwLock<Option<ZigbeeDeviceStoreData>>,
}

impl ZigbeeDeviceStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
            cache: RwLock::new(None),
        }
    }

    fn path(&self) -> PathBuf {
        self.root.join("zigbee_devices.json")
    }

    async fn load(&self) -> Result<ZigbeeDeviceStoreData> {
        let path = self.path();
        if !path.exists() {
            return Ok(ZigbeeDeviceStoreData::default());
        }
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))
    }

    async fn ensure_loaded(&self) -> Result<()> {
        if self.cache.read().await.is_none() {
            let data = self.load().await?;
            *self.cache.write().await = Some(data);
        }
        Ok(())
    }

    async fn save(&self, data: &ZigbeeDeviceStoreData) -> Result<()> {
        save_json_atomic(&self.path(), data).await
    }

    /// Insert or replace the record for a device (keyed on `ieee_addr`).
    pub async fn upsert(&self, record: ZigbeeDeviceRecord) -> Result<()> {
        let _guard = self.lock.lock().await;
        self.ensure_loaded().await?;
        let mut cache = self.cache.write().await;
        let data = cache.as_mut().expect("cache populated above");
        if let Some(existing) = data.devices.iter_mut().find(|d| d.ieee_addr == record.ieee_addr) {
            // Preserve user-set fields during an upsert from the bridge.
            let name_by_user = existing.name_by_user.clone();
            let user_area_id = existing.user_area_id.clone();
            *existing = record;
            existing.name_by_user = name_by_user;
            existing.user_area_id = user_area_id;
        } else {
            data.devices.push(record);
        }
        self.save(data).await
    }

    /// Return a single record by IEEE address, or `None`.
    pub async fn get_by_ieee(&self, ieee_addr: &str) -> Result<Option<ZigbeeDeviceRecord>> {
        self.ensure_loaded().await?;
        let cache = self.cache.read().await;
        Ok(cache
            .as_ref()
            .expect("cache populated above")
            .devices
            .iter()
            .find(|d| d.ieee_addr == ieee_addr)
            .cloned())
    }

    /// Return all device records, sorted by friendly name.
    pub async fn list(&self) -> Result<Vec<ZigbeeDeviceRecord>> {
        self.ensure_loaded().await?;
        let cache = self.cache.read().await;
        let mut devices = cache
            .as_ref()
            .expect("cache populated above")
            .devices
            .clone();
        devices.sort_by(|a, b| a.display_name().cmp(b.display_name()));
        Ok(devices)
    }

    /// Apply a partial metadata update (rename / area assignment).
    pub async fn update_meta(
        &self,
        ieee_addr: &str,
        update: ZigbeeDeviceMetaUpdate,
    ) -> Result<bool> {
        let _guard = self.lock.lock().await;
        self.ensure_loaded().await?;
        let mut cache = self.cache.write().await;
        let data = cache.as_mut().expect("cache populated above");
        let Some(dev) = data.devices.iter_mut().find(|d| d.ieee_addr == ieee_addr) else {
            return Ok(false);
        };
        if let Some(v) = update.name_by_user {
            dev.name_by_user = v;
        }
        if let Some(v) = update.user_area_id {
            dev.user_area_id = v;
        }
        self.save(data).await?;
        Ok(true)
    }

    /// Set the `last_seen` timestamp without touching user-set fields.
    ///
    /// Called on every `StateChanged` event so the device list shows freshness.
    pub async fn touch_last_seen(&self, ieee_addr: &str, ts: String) -> Result<bool> {
        let _guard = self.lock.lock().await;
        self.ensure_loaded().await?;
        let mut cache = self.cache.write().await;
        let data = cache.as_mut().expect("cache populated above");
        let Some(dev) = data.devices.iter_mut().find(|d| d.ieee_addr == ieee_addr) else {
            return Ok(false);
        };
        dev.last_seen = Some(ts);
        self.save(data).await?;
        Ok(true)
    }

    /// Remove a device record permanently.
    pub async fn remove(&self, ieee_addr: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        self.ensure_loaded().await?;
        let mut cache = self.cache.write().await;
        let data = cache.as_mut().expect("cache populated above");
        let before = data.devices.len();
        data.devices.retain(|d| d.ieee_addr != ieee_addr);
        if data.devices.len() == before {
            return Ok(false);
        }
        self.save(data).await?;
        Ok(true)
    }
}
