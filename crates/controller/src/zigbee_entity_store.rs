//! Persistent store for HA entities auto-derived from Zigbee devices.
//!
//! Each Zigbee device can produce one or more entities (e.g. a smart bulb
//! yields a `light.*` entity; a multi-sensor yields `sensor.*` entities for
//! temperature, humidity, etc.).  Entities are re-derived from ZCL cluster
//! lists on interview completion and stored here until the device is removed.
//!
//! Follows the same `Mutex + RwLock + JSON file` pattern as other stores.
//!
//! File: `<data_dir>/zigbee_entities.json`

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::storage::save_json_atomic;

/// A single entity record derived from a Zigbee device's ZCL clusters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZigbeeEntityRecord {
    /// Home Assistant entity ID in `domain.object_id` form
    /// (e.g. `"light.living_room_bulb"`).
    pub entity_id: String,
    /// IEEE address of the parent Zigbee device.
    pub ieee_addr: String,
    /// HA platform domain: `light`, `switch`, `sensor`, `binary_sensor`.
    pub domain: String,
    /// For `sensor` and `binary_sensor` entities: the key to read from the
    /// device's raw ZCL state map (e.g. `"temperature"`).  `None` for
    /// `light` and `switch` which use the `"state"` key directly.
    #[serde(default)]
    pub attribute_key: Option<String>,
    /// HA device class (e.g. `"temperature"`, `"occupancy"`).
    #[serde(default)]
    pub device_class: Option<String>,
    /// Unit of measurement (e.g. `"°C"`, `"%"`, `"lx"`).
    #[serde(default)]
    pub unit_of_measurement: Option<String>,
    /// User-set display name overriding the auto-generated entity name.
    #[serde(default)]
    pub name_by_user: Option<String>,
    /// Area/room assigned by the user.
    #[serde(default)]
    pub user_area_id: Option<String>,
}

impl ZigbeeEntityRecord {
    /// Display name: user override → entity_id.
    pub fn display_name(&self) -> &str {
        self.name_by_user.as_deref().unwrap_or(&self.entity_id)
    }
}

/// Partial update applied via the entity rename / area-assign API.
pub struct ZigbeeEntityMetaUpdate {
    pub name_by_user: Option<Option<String>>,
    pub user_area_id: Option<Option<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ZigbeeEntityStoreData {
    /// Keyed by entity_id for O(1) lookup.
    entities: BTreeMap<String, ZigbeeEntityRecord>,
}

/// Thread-safe, lazily-loaded store for Zigbee entity records.
pub struct ZigbeeEntityStore {
    root: PathBuf,
    lock: Mutex<()>,
    cache: RwLock<Option<ZigbeeEntityStoreData>>,
}

impl ZigbeeEntityStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
            cache: RwLock::new(None),
        }
    }

    fn path(&self) -> PathBuf {
        self.root.join("zigbee_entities.json")
    }

    async fn load(&self) -> Result<ZigbeeEntityStoreData> {
        let path = self.path();
        if !path.exists() {
            return Ok(ZigbeeEntityStoreData::default());
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

    async fn save(&self, data: &ZigbeeEntityStoreData) -> Result<()> {
        save_json_atomic(&self.path(), data).await
    }

    /// Replace all entities for a device with the provided list.
    ///
    /// Preserves `name_by_user` and `user_area_id` for any entity whose
    /// `entity_id` already exists in the store.
    pub async fn register_bulk(
        &self,
        records: Vec<ZigbeeEntityRecord>,
    ) -> Result<()> {
        let _guard = self.lock.lock().await;
        self.ensure_loaded().await?;
        let mut cache = self.cache.write().await;
        let data = cache.as_mut().expect("cache populated above");
        for mut rec in records {
            // Preserve user-set fields when re-registering after a rejoin.
            if let Some(existing) = data.entities.get(&rec.entity_id) {
                rec.name_by_user = existing.name_by_user.clone();
                rec.user_area_id = existing.user_area_id.clone();
            }
            data.entities.insert(rec.entity_id.clone(), rec);
        }
        self.save(data).await
    }

    /// Return all entity records.
    pub async fn list(&self) -> Result<Vec<ZigbeeEntityRecord>> {
        self.ensure_loaded().await?;
        let cache = self.cache.read().await;
        Ok(cache
            .as_ref()
            .expect("cache populated above")
            .entities
            .values()
            .cloned()
            .collect())
    }

    /// Return all entities belonging to a specific Zigbee device.
    pub async fn list_for_device(&self, ieee_addr: &str) -> Result<Vec<ZigbeeEntityRecord>> {
        self.ensure_loaded().await?;
        let cache = self.cache.read().await;
        Ok(cache
            .as_ref()
            .expect("cache populated above")
            .entities
            .values()
            .filter(|e| e.ieee_addr == ieee_addr)
            .cloned()
            .collect())
    }

    /// Look up a single entity by entity_id.
    pub async fn get(&self, entity_id: &str) -> Result<Option<ZigbeeEntityRecord>> {
        self.ensure_loaded().await?;
        let cache = self.cache.read().await;
        Ok(cache
            .as_ref()
            .expect("cache populated above")
            .entities
            .get(entity_id)
            .cloned())
    }

    /// Apply a partial metadata update (rename / area assignment).
    pub async fn update_meta(
        &self,
        entity_id: &str,
        update: ZigbeeEntityMetaUpdate,
    ) -> Result<bool> {
        let _guard = self.lock.lock().await;
        self.ensure_loaded().await?;
        let mut cache = self.cache.write().await;
        let data = cache.as_mut().expect("cache populated above");
        let Some(ent) = data.entities.get_mut(entity_id) else {
            return Ok(false);
        };
        if let Some(v) = update.name_by_user {
            ent.name_by_user = v;
        }
        if let Some(v) = update.user_area_id {
            ent.user_area_id = v;
        }
        self.save(data).await?;
        Ok(true)
    }

    /// Remove all entities for a given device.  Returns the count removed.
    pub async fn remove_for_device(&self, ieee_addr: &str) -> Result<usize> {
        let _guard = self.lock.lock().await;
        self.ensure_loaded().await?;
        let mut cache = self.cache.write().await;
        let data = cache.as_mut().expect("cache populated above");
        let before = data.entities.len();
        data.entities.retain(|_, v| v.ieee_addr != ieee_addr);
        let removed = before - data.entities.len();
        if removed > 0 {
            self.save(data).await?;
        }
        Ok(removed)
    }
}
