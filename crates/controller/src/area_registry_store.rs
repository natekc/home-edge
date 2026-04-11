use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::storage::save_json_atomic;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A single area entry, mirroring the HA core area registry entry shape.
///
/// Source: homeassistant/components/area_registry.py  AreaEntry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredArea {
    pub area_id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub floor_id: Option<String>,
    pub icon: Option<String>,
    pub picture: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AreaRegistryData {
    areas: Vec<StoredArea>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Persistent, in-memory-cached area registry.
///
/// Persists to `<data_dir>/area_registry.json`.  On first boot the store is
/// seeded from the static `[areas]` section of `config.toml` so that existing
/// deployments keep working.  After that config.toml is ignored for areas —
/// all mutations go through the WS API or the store methods.
pub struct AreaRegistryStore {
    root: PathBuf,
    lock: Mutex<()>,
    cache: RwLock<Option<AreaRegistryData>>,
}

impl AreaRegistryStore {
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

    pub async fn list(&self) -> Result<Vec<StoredArea>> {
        Ok(self.load_data().await?.areas)
    }

    /// Seed from a static name list if the registry file doesn't exist yet.
    /// No-op when the registry already has entries.
    pub async fn seed_if_empty(&self, names: &[String]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;
        if !data.areas.is_empty() {
            return Ok(());
        }
        data.areas = names
            .iter()
            .map(|name| StoredArea {
                area_id: slugify(name),
                name: name.clone(),
                aliases: vec![],
                floor_id: None,
                icon: None,
                picture: None,
            })
            .collect();
        self.save(&data).await
    }

    /// Create a new area with the given name.
    ///
    /// The `area_id` is derived from the name via `slugify`.  If a collision
    /// occurs a short UUID suffix is appended to keep it unique.
    pub async fn create(&self, name: String) -> Result<StoredArea> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;

        let base = slugify(&name);
        let area_id = if data.areas.iter().any(|a| a.area_id == base) {
            format!("{}_{}", base, &Uuid::new_v4().simple().to_string()[..8])
        } else {
            base
        };

        let area = StoredArea {
            area_id,
            name,
            aliases: vec![],
            floor_id: None,
            icon: None,
            picture: None,
        };
        data.areas.push(area.clone());
        self.save(&data).await?;
        Ok(area)
    }

    /// Update an existing area.
    ///
    /// Each field is `Option`-wrapped: `None` means "leave unchanged", `Some`
    /// means "set to this value" (including `Some(None)` to clear an optional
    /// field).  Returns `None` if no area with `area_id` exists.
    pub async fn update(
        &self,
        area_id: &str,
        name: Option<String>,
        aliases: Option<Vec<String>>,
        floor_id: Option<Option<String>>,
        icon: Option<Option<String>>,
        picture: Option<Option<String>>,
    ) -> Result<Option<StoredArea>> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;

        let Some(area) = data.areas.iter_mut().find(|a| a.area_id == area_id) else {
            return Ok(None);
        };
        if let Some(n) = name {
            area.name = n;
        }
        if let Some(a) = aliases {
            area.aliases = a;
        }
        if let Some(f) = floor_id {
            area.floor_id = f;
        }
        if let Some(i) = icon {
            area.icon = i;
        }
        if let Some(p) = picture {
            area.picture = p;
        }
        let updated = area.clone();
        self.save(&data).await?;
        Ok(Some(updated))
    }

    /// Delete an area by id.  Returns `true` if it existed, `false` if not.
    pub async fn delete(&self, area_id: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;
        let before = data.areas.len();
        data.areas.retain(|a| a.area_id != area_id);
        if data.areas.len() == before {
            return Ok(false);
        }
        self.save(&data).await?;
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn save(&self, data: &AreaRegistryData) -> Result<()> {
        save_json_atomic(&self.path(), data).await?;
        *self.cache.write().await = Some(data.clone());
        Ok(())
    }

    fn path(&self) -> PathBuf {
        self.root.join("area_registry.json")
    }

    async fn load_data(&self) -> Result<AreaRegistryData> {
        if let Some(data) = self.cache.read().await.clone() {
            return Ok(data);
        }
        let _guard = self.lock.lock().await;
        self.load_locked().await
    }

    async fn load_locked(&self) -> Result<AreaRegistryData> {
        if let Some(data) = self.cache.read().await.clone() {
            return Ok(data);
        }
        let path = self.path();
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(AreaRegistryData::default())
            }
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }?;
        *self.cache.write().await = Some(data.clone());
        Ok(data)
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Convert an area name to a URL-safe, lowercase, underscore-separated id.
///
/// Mirrors the slug used by HA's `area_registry.py` for auto-generated ids.
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

    fn store() -> AreaRegistryStore {
        AreaRegistryStore::new(temp_dir("area-registry"))
    }

    #[tokio::test]
    async fn empty_on_first_load() {
        let s = store();
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn seed_populates_from_names() {
        let s = store();
        s.seed_if_empty(&["Living Room".into(), "Kitchen".into()])
            .await
            .unwrap();
        let areas = s.list().await.unwrap();
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0].area_id, "living_room");
        assert_eq!(areas[0].name, "Living Room");
    }

    #[tokio::test]
    async fn seed_is_noop_when_registry_has_entries() {
        let s = store();
        s.create("Existing".into()).await.unwrap();
        s.seed_if_empty(&["Should not appear".into()])
            .await
            .unwrap();
        let names: Vec<_> = s.list().await.unwrap().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["Existing"]);
    }

    #[tokio::test]
    async fn create_persists_and_caches() {
        let root = temp_dir("area-create");
        let s1 = AreaRegistryStore::new(root.clone());
        let area = s1.create("Bedroom".into()).await.unwrap();
        assert_eq!(area.area_id, "bedroom");

        // Fresh store reads the same data from disk.
        let s2 = AreaRegistryStore::new(root);
        let areas = s2.list().await.unwrap();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].name, "Bedroom");
    }

    #[tokio::test]
    async fn create_deduplicates_area_id() {
        let s = store();
        let a1 = s.create("Office".into()).await.unwrap();
        let a2 = s.create("Office".into()).await.unwrap();
        assert_ne!(a1.area_id, a2.area_id);
        assert!(a2.area_id.starts_with("office_"));
    }

    #[tokio::test]
    async fn update_changes_name() {
        let s = store();
        let area = s.create("Old Name".into()).await.unwrap();
        let updated = s
            .update(&area.area_id, Some("New Name".into()), None, None, None, None)
            .await
            .unwrap();
        assert_eq!(updated.unwrap().name, "New Name");
    }

    #[tokio::test]
    async fn update_unknown_returns_none() {
        let s = store();
        let result = s
            .update("no_such_id", Some("X".into()), None, None, None, None)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_removes_area() {
        let s = store();
        let area = s.create("Garage".into()).await.unwrap();
        assert!(s.delete(&area.area_id).await.unwrap());
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_unknown_returns_false() {
        let s = store();
        assert!(!s.delete("no_such_id").await.unwrap());
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Living Room"), "living_room");
        assert_eq!(slugify("Büro"), "büro"); // non-ascii letters pass through
        assert_eq!(slugify("2nd Floor!"), "2nd_floor");
    }
}
