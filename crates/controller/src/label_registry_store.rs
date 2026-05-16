use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::storage::save_json_atomic;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A single label entry.
///
/// Source: homeassistant/helpers/label_registry.py  LabelEntry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LabelEntry {
    pub label_id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LabelRegistryData {
    labels: Vec<LabelEntry>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Persistent, in-memory-cached label registry.
///
/// Persists to `<data_dir>/label_registry.json`.
pub struct LabelRegistryStore {
    root: PathBuf,
    lock: Mutex<()>,
    cache: RwLock<Option<LabelRegistryData>>,
}

impl LabelRegistryStore {
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

    pub async fn list(&self) -> Result<Vec<LabelEntry>> {
        Ok(self.load_data().await?.labels)
    }

    /// Create a new label.
    ///
    /// Source: homeassistant/components/config/label_registry.py  websocket_create_label
    pub async fn create(
        &self,
        name: String,
        description: Option<String>,
        icon: Option<String>,
        color: Option<String>,
    ) -> Result<LabelEntry> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;

        let base = slugify(&name);
        let label_id = if data.labels.iter().any(|l| l.label_id == base) {
            format!("{}_{}", base, &Uuid::new_v4().simple().to_string()[..8])
        } else {
            base
        };

        let entry = LabelEntry {
            label_id,
            name,
            description,
            icon,
            color,
        };
        data.labels.push(entry.clone());
        self.save(&data).await?;
        Ok(entry)
    }

    /// Update an existing label.
    ///
    /// Each field is `Option`-wrapped: `None` means "leave unchanged".
    /// `Some(None)` clears an optional field.
    /// Returns `None` if no label with `label_id` exists.
    ///
    /// Source: homeassistant/components/config/label_registry.py  websocket_update_label
    pub async fn update(
        &self,
        label_id: &str,
        name: Option<String>,
        description: Option<Option<String>>,
        icon: Option<Option<String>>,
        color: Option<Option<String>>,
    ) -> Result<Option<LabelEntry>> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;

        let Some(entry) = data.labels.iter_mut().find(|l| l.label_id == label_id) else {
            return Ok(None);
        };
        if let Some(n) = name {
            entry.name = n;
        }
        if let Some(d) = description {
            entry.description = d;
        }
        if let Some(i) = icon {
            entry.icon = i;
        }
        if let Some(c) = color {
            entry.color = c;
        }
        let updated = entry.clone();
        self.save(&data).await?;
        Ok(Some(updated))
    }

    /// Delete a label by id.  Returns `true` if it existed, `false` if not.
    ///
    /// Source: homeassistant/components/config/label_registry.py  websocket_delete_label
    pub async fn delete(&self, label_id: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let mut data = self.load_locked().await?;
        let before = data.labels.len();
        data.labels.retain(|l| l.label_id != label_id);
        if data.labels.len() == before {
            return Ok(false);
        }
        self.save(&data).await?;
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn save(&self, data: &LabelRegistryData) -> Result<()> {
        save_json_atomic(&self.path(), data).await?;
        *self.cache.write().await = Some(data.clone());
        Ok(())
    }

    fn path(&self) -> PathBuf {
        self.root.join("label_registry.json")
    }

    async fn load_data(&self) -> Result<LabelRegistryData> {
        if let Some(data) = self.cache.read().await.clone() {
            return Ok(data);
        }
        let _guard = self.lock.lock().await;
        self.load_locked().await
    }

    async fn load_locked(&self) -> Result<LabelRegistryData> {
        if let Some(data) = self.cache.read().await.clone() {
            return Ok(data);
        }
        let path = self.path();
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(LabelRegistryData::default())
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

/// Convert a label name to a URL-safe, lowercase, underscore-separated id.
fn slugify(name: &str) -> String {
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

    fn store() -> LabelRegistryStore {
        LabelRegistryStore::new(temp_dir("label-registry"))
    }

    #[tokio::test]
    async fn create_and_list() {
        let s = store();
        let entry = s.create("Living Room".into(), None, None, None).await.unwrap();
        assert_eq!(entry.label_id, "living_room");
        assert_eq!(entry.name, "Living Room");

        let labels = s.list().await.unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].label_id, "living_room");
    }

    #[tokio::test]
    async fn duplicate_name_gets_suffix() {
        let s = store();
        let a = s.create("Test".into(), None, None, None).await.unwrap();
        let b = s.create("Test".into(), None, None, None).await.unwrap();
        assert_ne!(a.label_id, b.label_id);
        assert!(b.label_id.starts_with("test_"));
    }

    #[tokio::test]
    async fn update_fields() {
        let s = store();
        let entry = s.create("Foo".into(), None, None, None).await.unwrap();
        let updated = s
            .update(&entry.label_id, Some("Bar".into()), None, Some(Some("mdi:tag".into())), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "Bar");
        assert_eq!(updated.icon.as_deref(), Some("mdi:tag"));
    }

    #[tokio::test]
    async fn delete_existing() {
        let s = store();
        let entry = s.create("Delete Me".into(), None, None, None).await.unwrap();
        assert!(s.delete(&entry.label_id).await.unwrap());
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_missing_returns_false() {
        let s = store();
        assert!(!s.delete("nonexistent").await.unwrap());
    }
}
