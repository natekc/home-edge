//! Zone storage — persists user-defined zones to `<data_dir>/zone.json`.
//!
//! # Parity with core
//!
//! In HomeAssistant core:
//! - `ZoneStorageCollection` stores user-defined zones in `zone.json`
//!   (storage key `"zone"`, version 1).
//! - `zone.home` is a **synthetic** entity derived from `hass.config`
//!   (latitude / longitude / radius / location_name), **not** from
//!   `ZoneStorageCollection`.
//! - `get_zones` webhook returns **all** zone states including `zone.home`.
//! - WS `zone/list` returns only `zone.json` items — no `zone.home`.
//!
//! home-edge follows the same model:
//! - [`ZoneStore`] holds only user-defined zones; starts **empty** on first boot.
//! - [`home_zone_state`] synthesises `zone.home` at the call site from
//!   `OnboardingState`, matching core's `_home_conf(hass)` pattern.
//!
//! Source: homeassistant/components/zone/__init__.py

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::storage::{OnboardingState, save_json_atomic};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A single user-defined zone, mirroring HA core's zone storage entry.
///
/// Source: homeassistant/components/zone/__init__.py  CREATE_FIELDS / UPDATE_FIELDS
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredZone {
    /// URL-safe slug derived from the zone name (`"work"`, `"school"`, …).
    /// Doubles as the entity_id suffix: `"zone.<zone_id>"`.
    pub zone_id: String,
    /// Human-readable display name.
    pub name: String,
    /// WGS-84 latitude; `None` until the user fills it in.
    pub latitude: Option<f64>,
    /// WGS-84 longitude; `None` until the user fills it in.
    pub longitude: Option<f64>,
    /// Geofence radius in metres. Default 100 m matches HA core `DEFAULT_RADIUS`.
    #[serde(default = "default_radius")]
    pub radius: f64,
    /// When `true` the zone is excluded from location-tracking notifications.
    /// Mirrors `CONF_PASSIVE` in core.
    #[serde(default)]
    pub passive: bool,
    /// Optional MDI icon string, e.g. `"mdi:briefcase"`.
    pub icon: Option<String>,
}

fn default_radius() -> f64 {
    100.0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ZoneStoreData {
    zones: Vec<StoredZone>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Persistent, in-memory-cached zone store.
///
/// Persists to `<data_dir>/zone.json`, matching core's `STORAGE_KEY = "zone"`.
/// Starts **empty** on first boot — no zones are pre-seeded.
/// `zone.home` is never stored here; use [`home_zone_state`] to synthesise it.
pub struct ZoneStore {
    root: PathBuf,
    lock: Mutex<()>,
    cache: RwLock<Option<ZoneStoreData>>,
}

impl ZoneStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
            cache: RwLock::new(None),
        }
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    pub async fn list(&self) -> Result<Vec<StoredZone>> {
        Ok(self.load_data().await?.zones)
    }

    /// Create a new zone. The `zone_id` is slugified from `name`; a short UUID
    /// suffix is appended on collision.
    pub async fn create(
        &self,
        name: String,
        latitude: Option<f64>,
        longitude: Option<f64>,
        radius: Option<f64>,
        passive: Option<bool>,
        icon: Option<String>,
    ) -> Result<StoredZone> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;

        let base = slugify(&name);
        // Guard: a name composed entirely of non-alphanumeric characters (e.g. "!!!",
        // pure emoji) would produce an empty slug, yielding an invalid entity_id `"zone."`.
        if base.trim_matches('_').is_empty() {
            return Err(anyhow::anyhow!(
                "zone name {:?} produces an empty zone_id; use alphanumeric characters",
                name
            ));
        }
        let zone_id = if data.zones.iter().any(|z| z.zone_id == base) {
            format!("{}_{}", base, &Uuid::new_v4().simple().to_string()[..8])
        } else {
            base
        };

        let zone = StoredZone {
            zone_id,
            name,
            latitude,
            longitude,
            radius: radius.unwrap_or_else(default_radius),
            passive: passive.unwrap_or(false),
            icon,
        };
        data.zones.push(zone.clone());
        self.save(&data).await?;
        Ok(zone)
    }

    /// Update an existing zone by `zone_id`.
    ///
    /// `None` means "leave unchanged"; `Some(None)` clears an optional field.
    /// Returns `None` if no zone with the given id exists.
    pub async fn update(
        &self,
        zone_id: &str,
        name: Option<String>,
        latitude: Option<Option<f64>>,
        longitude: Option<Option<f64>>,
        radius: Option<f64>,
        passive: Option<bool>,
        icon: Option<Option<String>>,
    ) -> Result<Option<StoredZone>> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;

        let Some(zone) = data.zones.iter_mut().find(|z| z.zone_id == zone_id) else {
            return Ok(None);
        };
        if let Some(n) = name {
            zone.name = n;
        }
        if let Some(lat) = latitude {
            zone.latitude = lat;
        }
        if let Some(lon) = longitude {
            zone.longitude = lon;
        }
        if let Some(r) = radius {
            zone.radius = r;
        }
        if let Some(p) = passive {
            zone.passive = p;
        }
        if let Some(i) = icon {
            zone.icon = i;
        }
        let updated = zone.clone();
        self.save(&data).await?;
        Ok(Some(updated))
    }

    /// Delete a zone. Returns `true` if it existed.
    ///
    /// Callers should guard `zone_id == "home"` before calling — `zone.home` is
    /// never in this store so it returns `false`, but an explicit guard makes
    /// intent clear.
    pub async fn delete(&self, zone_id: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;
        let before = data.zones.len();
        data.zones.retain(|z| z.zone_id != zone_id);
        if data.zones.len() == before {
            return Ok(false);
        }
        self.save(&data).await?;
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn save(&self, data: &ZoneStoreData) -> Result<()> {
        save_json_atomic(&self.path(), data).await?;
        *self.cache.write().await = Some(data.clone());
        Ok(())
    }

    fn path(&self) -> PathBuf {
        self.root.join("zone.json")
    }

    async fn load_data(&self) -> Result<ZoneStoreData> {
        if let Some(data) = self.cache.read().await.clone() {
            return Ok(data);
        }
        let _guard = self.lock.lock().await;
        self.load_locked().await
    }

    async fn load_locked(&self) -> Result<ZoneStoreData> {
        if let Some(data) = self.cache.read().await.clone() {
            return Ok(data);
        }
        let path = self.path();
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(ZoneStoreData::default())
            }
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }?;
        *self.cache.write().await = Some(data.clone());
        Ok(data)
    }
}

// ---------------------------------------------------------------------------
// Wire format helpers
// ---------------------------------------------------------------------------

/// Serialise a [`StoredZone`] into the HA state wire format used by the
/// `get_zones` webhook.
///
/// The format matches what `hass.states.get(entity_id).as_dict()` returns for
/// a zone entity. `state` is the person-count string; we always emit `"0"`
/// since home-edge has no person/presence tracking yet.
///
/// Source: homeassistant/components/zone/__init__.py  Zone._generate_attrs
pub fn zone_to_state(zone: &StoredZone) -> Value {
    json!({
        "entity_id": format!("zone.{}", zone.zone_id),
        "state": "0",
        "attributes": {
            "friendly_name": zone.name,
            "latitude": zone.latitude,
            "longitude": zone.longitude,
            "radius": zone.radius,
            "passive": zone.passive,
            "persons": [],
            "editable": true,
            "icon": zone.icon,
        },
        "context": {"id": "00000000000000000000000000000000", "parent_id": null, "user_id": null},
        "last_changed": "1970-01-01T00:00:00.000000+00:00",
        "last_updated": "1970-01-01T00:00:00.000000+00:00",
    })
}

/// Synthesise the `zone.home` state from `OnboardingState`.
///
/// `zone.home` is **never** stored in `zone.json`; it is derived at runtime
/// from the system configuration, mirroring core's `_home_conf(hass)` pattern
/// where `zone.home` is a config-derived entity, not a storage collection item.
///
/// Source: homeassistant/components/zone/__init__.py  _home_conf + async_setup
pub fn home_zone_state(onboarding: &OnboardingState) -> Value {
    let name = onboarding.location_name.as_deref().unwrap_or("Home");
    json!({
        "entity_id": "zone.home",
        "state": "0",
        "attributes": {
            "friendly_name": name,
            "latitude": onboarding.latitude,
            "longitude": onboarding.longitude,
            "radius": onboarding.radius,
            "passive": false,
            "persons": [],
            "editable": true,
            "icon": "mdi:home",
        },
        "context": {"id": "00000000000000000000000000000000", "parent_id": null, "user_id": null},
        "last_changed": "1970-01-01T00:00:00.000000+00:00",
        "last_updated": "1970-01-01T00:00:00.000000+00:00",
    })
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Convert a zone name to a URL-safe, lowercase, underscore-separated id.
/// Mirrors the slug pattern used across home-edge registries.
pub(crate) fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c == ' ' { '_' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::temp_dir;

    fn store() -> ZoneStore {
        ZoneStore::new(temp_dir("zone-store"))
    }

    #[tokio::test]
    async fn empty_on_first_boot() {
        assert!(store().list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_and_list() {
        let s = store();
        let z = s
            .create("Work".into(), Some(51.5), Some(-0.1), None, None, None)
            .await
            .unwrap();
        assert_eq!(z.zone_id, "work");
        assert_eq!(z.radius, 100.0);
        assert!(!z.passive);
        assert_eq!(s.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_deduplicates_zone_id() {
        let s = store();
        let z1 = s.create("Work".into(), None, None, None, None, None).await.unwrap();
        let z2 = s.create("Work".into(), None, None, None, None, None).await.unwrap();
        assert_ne!(z1.zone_id, z2.zone_id);
        assert!(z2.zone_id.starts_with("work_"));
    }

    #[tokio::test]
    async fn update_fields() {
        let s = store();
        let z = s.create("Work".into(), None, None, None, None, None).await.unwrap();
        let updated = s
            .update(&z.zone_id, None, Some(Some(51.5)), Some(Some(-0.1)), Some(200.0), None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.latitude, Some(51.5));
        assert_eq!(updated.radius, 200.0);
    }

    #[tokio::test]
    async fn update_returns_none_for_missing() {
        assert!(store().update("nonexistent", None, None, None, None, None, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_existing() {
        let s = store();
        let z = s.create("Work".into(), None, None, None, None, None).await.unwrap();
        assert!(s.delete(&z.zone_id).await.unwrap());
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_home_returns_false() {
        // zone.home is synthetic — never in the store.
        assert!(!store().delete("home").await.unwrap());
    }

    #[tokio::test]
    async fn persists_across_reload() {
        let root = temp_dir("zone-persist");
        ZoneStore::new(root.clone())
            .create("School".into(), None, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(ZoneStore::new(root).list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn home_zone_state_uses_onboarding_fields() {
        let o = OnboardingState {
            location_name: Some("Test Home".into()),
            latitude: Some(48.8566),
            longitude: Some(2.3522),
            radius: 150.0,
            ..Default::default()
        };
        let state = home_zone_state(&o);
        assert_eq!(state["entity_id"], "zone.home");
        assert_eq!(state["attributes"]["friendly_name"], "Test Home");
        assert_eq!(state["attributes"]["latitude"], 48.8566);
        assert_eq!(state["attributes"]["radius"], 150.0);
        assert_eq!(state["attributes"]["icon"], "mdi:home");
    }

    #[tokio::test]
    async fn zone_to_state_format() {
        let z = StoredZone {
            zone_id: "work".into(),
            name: "Work".into(),
            latitude: Some(51.5),
            longitude: Some(-0.1),
            radius: 200.0,
            passive: false,
            icon: Some("mdi:briefcase".into()),
        };
        let state = zone_to_state(&z);
        assert_eq!(state["entity_id"], "zone.work");
        assert_eq!(state["state"], "0");
        assert_eq!(state["attributes"]["friendly_name"], "Work");
        assert_eq!(state["attributes"]["radius"], 200.0);
        assert_eq!(state["attributes"]["editable"], true);
    }

    #[tokio::test]
    async fn zone_to_state_null_coords_are_null_in_json() {
        let z = StoredZone {
            zone_id: "draft".into(),
            name: "Draft".into(),
            latitude: None,
            longitude: None,
            radius: 100.0,
            passive: false,
            icon: None,
        };
        let state = zone_to_state(&z);
        assert!(state["attributes"]["latitude"].is_null());
        assert!(state["attributes"]["longitude"].is_null());
    }

    #[tokio::test]
    async fn create_rejects_empty_slug() {
        // Names whose slug is empty (only non-alphanumeric chars) must be rejected.
        let s = store();
        let err = s.create("!!!".into(), None, None, None, None, None).await;
        assert!(err.is_err(), "should reject name that slugifies to empty");
    }

    #[test]
    fn slugify_ascii() {
        assert_eq!(slugify("Living Room"), "living_room");
        assert_eq!(slugify("Work"), "work");
        assert_eq!(slugify("My Home!"), "my_home");
    }

    #[test]
    fn slugify_all_special_chars_produces_empty() {
        assert_eq!(slugify("!!!"), "");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn slugify_unicode_alphanumeric_passes_through() {
        // Rust's is_alphanumeric() returns true for non-ASCII letters (é, ü, 中, …).
        // This matches the permissive slugification used in area_registry_store.
        let result = slugify("café");
        assert!(!result.is_empty());
        assert!(result.chars().all(|c| c.is_alphanumeric() || c == '_'));
    }
}
