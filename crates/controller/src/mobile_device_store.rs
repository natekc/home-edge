use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::storage::save_json_atomic;

#[derive(Debug, Clone)]
pub struct MobileDeviceRegistration {
    pub app_id: String,
    pub app_name: String,
    pub app_version: String,
    pub device_name: String,
    pub manufacturer: String,
    pub model: String,
    pub os_name: String,
    pub os_version: Option<String>,
    pub device_id: Option<String>,
    pub supports_encryption: bool,
    pub owner_username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MobileDeviceRecord {
    pub webhook_id: String,
    pub secret: Option<String>,
    pub app_id: String,
    pub app_name: String,
    pub app_version: String,
    pub device_name: String,
    pub manufacturer: String,
    pub model: String,
    pub os_name: String,
    pub os_version: Option<String>,
    pub device_id: Option<String>,
    pub supports_encryption: bool,
    pub owner_username: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MobileDeviceStoreData {
    devices: Vec<MobileDeviceRecord>,
}

pub struct MobileDeviceStore {
    root: PathBuf,
    lock: Mutex<()>,
}

impl MobileDeviceStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
        }
    }

    pub async fn register(
        &self,
        registration: MobileDeviceRegistration,
    ) -> Result<MobileDeviceRecord> {
        let _guard = self.lock.lock().await;
        let path = self.path();
        let mut data = self.load_data().await?;

        if let Some(device_id) = registration.device_id.as_deref() {
            if let Some(index) = data.devices.iter().position(|device| {
                device.device_id.as_deref() == Some(device_id)
                    && device.app_id == registration.app_id
            }) {
                {
                    let existing = &mut data.devices[index];
                    existing.app_name = registration.app_name;
                    existing.app_version = registration.app_version;
                    existing.device_name = registration.device_name;
                    existing.manufacturer = registration.manufacturer;
                    existing.model = registration.model;
                    existing.os_name = registration.os_name;
                    existing.os_version = registration.os_version;
                    existing.supports_encryption = registration.supports_encryption;
                    existing.owner_username = registration.owner_username;
                    if existing.secret.is_none() && existing.supports_encryption {
                        existing.secret = Some(new_secret());
                    }
                }
                save_json_atomic(&path, &data).await?;
                return Ok(data.devices[index].clone());
            }
        }

        let record = MobileDeviceRecord {
            webhook_id: new_webhook_id(),
            secret: registration.supports_encryption.then(new_secret),
            app_id: registration.app_id,
            app_name: registration.app_name,
            app_version: registration.app_version,
            device_name: registration.device_name,
            manufacturer: registration.manufacturer,
            model: registration.model,
            os_name: registration.os_name,
            os_version: registration.os_version,
            device_id: registration.device_id,
            supports_encryption: registration.supports_encryption,
            owner_username: registration.owner_username,
        };
        data.devices.push(record.clone());
        save_json_atomic(&path, &data).await?;
        Ok(record)
    }

    pub async fn all(&self) -> Result<Vec<MobileDeviceRecord>> {
        Ok(self.load_data().await?.devices)
    }

    fn path(&self) -> PathBuf {
        self.root.join("mobile_devices.json")
    }

    async fn load_data(&self) -> Result<MobileDeviceStoreData> {
        let path = self.path();
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(MobileDeviceStoreData::default())
            }
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }
}

fn new_webhook_id() -> String {
    Uuid::new_v4().to_string().replace('-', "")
}

fn new_secret() -> String {
    format!(
        "{}{}",
        Uuid::new_v4().to_string().replace('-', ""),
        Uuid::new_v4().to_string().replace('-', "")
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(prefix: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("home-edge-{prefix}-{nanos}-{unique}"))
    }

    fn registration(device_id: Option<&str>) -> MobileDeviceRegistration {
        MobileDeviceRegistration {
            app_id: "io.homeassistant.ios".into(),
            app_name: "Home Assistant".into(),
            app_version: "2024.1".into(),
            device_name: "My iPhone".into(),
            manufacturer: "Apple".into(),
            model: "iPhone 15".into(),
            os_name: "iOS".into(),
            os_version: Some("17.0".into()),
            device_id: device_id.map(str::to_string),
            supports_encryption: false,
            owner_username: Some("owner".into()),
        }
    }

    #[tokio::test]
    async fn persists_mobile_devices() {
        let store = MobileDeviceStore::new(temp_dir("mobile-devices"));
        let record = store
            .register(registration(Some("device-1")))
            .await
            .expect("register");
        let all = store.all().await.expect("load devices");

        assert_eq!(all, vec![record]);
    }

    #[tokio::test]
    async fn reuses_device_registration_by_device_id() {
        let store = MobileDeviceStore::new(temp_dir("mobile-devices-reuse"));
        let first = store
            .register(registration(Some("device-1")))
            .await
            .expect("first");
        let second = store
            .register(registration(Some("device-1")))
            .await
            .expect("second");

        assert_eq!(first.webhook_id, second.webhook_id);
        assert_eq!(store.all().await.expect("load devices").len(), 1);
    }
}
