use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::storage::save_json_atomic;

#[derive(Debug, Clone)]
pub struct MobileEntityRegistration {
    pub webhook_id: String,
    pub entity_type: String,
    pub sensor_unique_id: String,
    pub sensor_name: String,
    pub device_class: Option<String>,
    pub unit_of_measurement: Option<String>,
    pub icon: Option<String>,
    pub entity_category: Option<String>,
    pub state_class: Option<String>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MobileEntityRecord {
    pub webhook_id: String,
    pub entity_type: String,
    pub sensor_unique_id: String,
    pub sensor_name: String,
    pub entity_id: String,
    pub device_class: Option<String>,
    pub unit_of_measurement: Option<String>,
    pub icon: Option<String>,
    pub entity_category: Option<String>,
    pub state_class: Option<String>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MobileEntityStoreData {
    entities: BTreeMap<String, MobileEntityRecord>,
}

pub struct MobileEntityStore {
    root: PathBuf,
    lock: Mutex<()>,
    cache: RwLock<Option<MobileEntityStoreData>>,
}

impl MobileEntityStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
            cache: RwLock::new(None),
        }
    }

    pub async fn register(
        &self,
        registration: MobileEntityRegistration,
    ) -> Result<MobileEntityRecord> {
        validate_registration(&registration)?;

        let _guard = self.lock.lock().await;
        let path = self.path();
        let mut data = self.load_locked().await?;
        let key = store_key(
            &registration.webhook_id,
            &registration.entity_type,
            &registration.sensor_unique_id,
        );

        let existing_entity_id = data
            .entities
            .get(&key)
            .map(|record| record.entity_id.clone());
        let record = MobileEntityRecord {
            webhook_id: registration.webhook_id,
            entity_type: registration.entity_type,
            sensor_unique_id: registration.sensor_unique_id,
            sensor_name: registration.sensor_name,
            entity_id: existing_entity_id.unwrap_or_else(|| derive_entity_id(&key)),
            device_class: registration.device_class,
            unit_of_measurement: registration.unit_of_measurement,
            icon: registration.icon,
            entity_category: registration.entity_category,
            state_class: registration.state_class,
            disabled: registration.disabled,
        };

        if data.entities.get(&key) == Some(&record) {
            return Ok(record);
        }

        data.entities.insert(key, record.clone());
        save_json_atomic(&path, &data).await?;
        *self.cache.write().await = Some(data.clone());
        Ok(record)
    }

    pub async fn get(
        &self,
        webhook_id: &str,
        entity_type: &str,
        sensor_unique_id: &str,
    ) -> Result<Option<MobileEntityRecord>> {
        let key = store_key(webhook_id, entity_type, sensor_unique_id);
        Ok(self.load_data().await?.entities.get(&key).cloned())
    }

    pub async fn list_by_webhook_id(&self, webhook_id: &str) -> Result<Vec<MobileEntityRecord>> {
        Ok(self
            .load_data()
            .await?
            .entities
            .into_values()
            .filter(|record| record.webhook_id == webhook_id)
            .collect())
    }

    pub async fn all(&self) -> Result<Vec<MobileEntityRecord>> {
        Ok(self.load_data().await?.entities.into_values().collect())
    }

    fn path(&self) -> PathBuf {
        self.root.join("mobile_entities.json")
    }

    async fn load_data(&self) -> Result<MobileEntityStoreData> {
        if let Some(data) = self.cache.read().await.clone() {
            return Ok(data);
        }

        let _guard = self.lock.lock().await;
        self.load_locked().await
    }

    async fn load_locked(&self) -> Result<MobileEntityStoreData> {
        if let Some(data) = self.cache.read().await.clone() {
            return Ok(data);
        }

        let path = self.path();
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(MobileEntityStoreData::default())
            }
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }?;
        *self.cache.write().await = Some(data.clone());
        Ok(data)
    }
}

fn validate_registration(registration: &MobileEntityRegistration) -> Result<()> {
    if registration.webhook_id.is_empty() {
        bail!("webhook_id must not be empty");
    }
    if registration.sensor_unique_id.is_empty() {
        bail!("sensor_unique_id must not be empty");
    }
    if registration.sensor_name.is_empty() {
        bail!("sensor_name must not be empty");
    }
    if !matches!(registration.entity_type.as_str(), "sensor" | "binary_sensor") {
        bail!("unsupported mobile entity type: {}", registration.entity_type);
    }
    Ok(())
}

fn store_key(webhook_id: &str, entity_type: &str, sensor_unique_id: &str) -> String {
    format!("{entity_type}:{webhook_id}:{sensor_unique_id}")
}

fn derive_entity_id(key: &str) -> String {
    let (entity_type, rest) = key
        .split_once(':')
        .expect("store key must include entity type separator");
    format!("{entity_type}.mobile_app_{}", sanitize_object_id(rest))
}

fn sanitize_object_id(value: &str) -> String {
    let mut object_id = String::with_capacity(value.len());
    let mut last_was_separator = false;

    for ch in value.chars() {
        let normalized = ch.to_ascii_lowercase();
        if normalized.is_ascii_lowercase() || normalized.is_ascii_digit() {
            object_id.push(normalized);
            last_was_separator = false;
            continue;
        }

        if !last_was_separator && !object_id.is_empty() {
            object_id.push('_');
            last_was_separator = true;
        }
    }

    while object_id.ends_with('_') {
        object_id.pop();
    }

    if object_id.is_empty() {
        "entity".to_string()
    } else {
        object_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::temp_dir;

    fn registration(
        webhook_id: &str,
        entity_type: &str,
        unique_id: &str,
    ) -> MobileEntityRegistration {
        MobileEntityRegistration {
            webhook_id: webhook_id.into(),
            entity_type: entity_type.into(),
            sensor_unique_id: unique_id.into(),
            sensor_name: "Battery Level".into(),
            device_class: Some("battery".into()),
            unit_of_measurement: Some("%".into()),
            icon: Some("mdi:battery".into()),
            entity_category: Some("diagnostic".into()),
            state_class: Some("measurement".into()),
            disabled: false,
        }
    }

    #[tokio::test]
    async fn persists_mobile_entities() {
        let root = temp_dir("mobile-entities");
        let store = MobileEntityStore::new(root.clone());
        let record = store
            .register(registration("webhook-1", "sensor", "battery_level"))
            .await
            .expect("register entity");

        let reloaded = MobileEntityStore::new(root)
            .all()
            .await
            .expect("load entities");

        assert_eq!(reloaded, vec![record]);
    }

    #[tokio::test]
    async fn reregister_updates_metadata_without_duplication() {
        let store = MobileEntityStore::new(temp_dir("mobile-entities-update"));
        let first = store
            .register(registration("webhook-1", "sensor", "battery_level"))
            .await
            .expect("first register");

        let mut updated = registration("webhook-1", "sensor", "battery_level");
        updated.sensor_name = "Phone Battery".into();
        updated.icon = Some("mdi:cellphone".into());
        updated.disabled = true;

        let second = store.register(updated).await.expect("second register");
        let all = store.all().await.expect("load entities");

        assert_eq!(all.len(), 1);
        assert_eq!(first.entity_id, second.entity_id);
        assert_eq!(second.sensor_name, "Phone Battery");
        assert_eq!(second.icon.as_deref(), Some("mdi:cellphone"));
        assert!(second.disabled);
    }

    #[tokio::test]
    async fn same_unique_id_under_different_webhooks_does_not_collide() {
        let store = MobileEntityStore::new(temp_dir("mobile-entities-collision"));
        let first = store
            .register(registration("webhook-1", "sensor", "battery_level"))
            .await
            .expect("first register");
        let second = store
            .register(registration("webhook-2", "sensor", "battery_level"))
            .await
            .expect("second register");

        assert_ne!(first.entity_id, second.entity_id);
        assert_eq!(store.all().await.expect("load entities").len(), 2);
    }

    #[tokio::test]
    async fn stable_entity_id_for_same_registration_key() {
        let store = MobileEntityStore::new(temp_dir("mobile-entities-stable-id"));
        let first = store
            .register(registration(
                "Webhook-ABC",
                "binary_sensor",
                "Motion-Sensor",
            ))
            .await
            .expect("first register");

        let mut updated = registration("Webhook-ABC", "binary_sensor", "Motion-Sensor");
        updated.sensor_name = "Front Door Motion".into();
        updated.device_class = Some("motion".into());

        let second = store.register(updated).await.expect("second register");

        assert_eq!(first.entity_id, second.entity_id);
        assert_eq!(
            second.entity_id,
            "binary_sensor.mobile_app_webhook_abc_motion_sensor"
        );
    }

    #[tokio::test]
    async fn list_by_webhook_id_filters_entities() {
        let store = MobileEntityStore::new(temp_dir("mobile-entities-list"));
        store
            .register(registration("webhook-1", "sensor", "battery_level"))
            .await
            .expect("register battery");
        store
            .register(registration("webhook-1", "binary_sensor", "motion"))
            .await
            .expect("register motion");
        store
            .register(registration("webhook-2", "sensor", "battery_level"))
            .await
            .expect("register other battery");

        let webhook_entities = store
            .list_by_webhook_id("webhook-1")
            .await
            .expect("list entities");

        assert_eq!(webhook_entities.len(), 2);
        assert!(
            webhook_entities
                .iter()
                .all(|entity| entity.webhook_id == "webhook-1")
        );
    }
}
