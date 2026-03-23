use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredUser {
    pub name: String,
    pub username: String,
    pub password: String,
    pub language: String,
}

#[derive(Debug)]
pub struct Storage {
    root: PathBuf,
    lock: Mutex<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OnboardingState {
    pub version: u32,
    pub onboarded: bool,
    pub updated_at_unix_ms: u128,
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub user: Option<StoredUser>,
    #[serde(default)]
    pub location_name: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub unit_system: Option<String>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            version: 1,
            onboarded: false,
            updated_at_unix_ms: now_unix_ms(),
            done: Vec::new(),
            user: None,
            location_name: None,
            country: None,
            language: None,
            time_zone: None,
            unit_system: None,
        }
    }
}

impl OnboardingState {
    pub fn step_done(&self, step: &str) -> bool {
        self.done.iter().any(|done| done == step)
    }
}

impl Storage {
    pub async fn new(root: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&root)
            .await
            .with_context(|| format!("failed to create data dir {}", root.display()))?;
        Ok(Self {
            root,
            lock: Mutex::new(()),
        })
    }

    /// Create an in-memory storage backed by a unique temp directory.
    /// Intended for tests only — avoids async setup in unit tests.
    #[cfg(test)]
    pub fn new_in_memory() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ha-compat-test-{}-{unique}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        // We create the dir synchronously via std::fs for test convenience.
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            lock: Mutex::new(()),
        }
    }

    pub async fn load_onboarding(&self) -> Result<OnboardingState> {
        let path = self.onboarding_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                let state: OnboardingState = serde_json::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                Ok(state)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(OnboardingState::default())
            }
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub async fn save_onboarding(&self, state: &OnboardingState) -> Result<()> {
        let _guard = self.lock.lock().await;
        let path = self.onboarding_path();
        save_json_atomic(&path, state).await
    }

    pub async fn update_onboarding<F>(&self, update: F) -> Result<OnboardingState>
    where
        F: FnOnce(&mut OnboardingState) -> Result<()>,
    {
        let _guard = self.lock.lock().await;
        let path = self.onboarding_path();
        let mut state = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => OnboardingState::default(),
            Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
        };

        update(&mut state)?;
        state.updated_at_unix_ms = now_unix_ms();
        save_json_atomic(&path, &state).await?;
        Ok(state)
    }

    pub async fn load_or_create_instance_id(&self) -> Result<String> {
        let _guard = self.lock.lock().await;
        let path = self.instance_id_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => Ok(contents.trim().to_string()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let instance_id = Uuid::new_v4().to_string();
                save_text_atomic(&path, &instance_id).await?;
                Ok(instance_id)
            }
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn onboarding_path(&self) -> PathBuf {
        self.root.join("onboarding.json")
    }

    fn instance_id_path(&self) -> PathBuf {
        self.root.join("instance_id")
    }
}

pub(crate) async fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("missing parent dir for {}", path.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed to create {}", parent.display()))?;

    let tmp_path = path.with_extension("tmp");
    let serialized = serde_json::to_vec_pretty(value).context("failed to serialize state")?;
    let final_parent = parent.to_path_buf();
    let final_path = path.to_path_buf();
    let final_tmp = tmp_path.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::fs::{self, File};
        use std::io::Write;

        let mut file = File::create(&final_tmp)
            .with_context(|| format!("failed to create {}", final_tmp.display()))?;
        file.write_all(&serialized)
            .with_context(|| format!("failed to write {}", final_tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", final_tmp.display()))?;
        fs::rename(&final_tmp, &final_path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                final_tmp.display(),
                final_path.display()
            )
        })?;

        File::open(&final_parent)
            .with_context(|| format!("failed to open {}", final_parent.display()))?
            .sync_all()
            .with_context(|| format!("failed to sync dir {}", final_parent.display()))?;
        Ok(())
    })
    .await
    .context("atomic write task failed")??;

    Ok(())
}

async fn save_text_atomic(path: &Path, value: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("missing parent dir for {}", path.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed to create {}", parent.display()))?;

    let tmp_path = path.with_extension("tmp");
    let serialized = value.as_bytes().to_vec();
    let final_parent = parent.to_path_buf();
    let final_path = path.to_path_buf();
    let final_tmp = tmp_path.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::fs::{self, File};
        use std::io::Write;

        let mut file = File::create(&final_tmp)
            .with_context(|| format!("failed to create {}", final_tmp.display()))?;
        file.write_all(&serialized)
            .with_context(|| format!("failed to write {}", final_tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", final_tmp.display()))?;
        fs::rename(&final_tmp, &final_path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                final_tmp.display(),
                final_path.display()
            )
        })?;

        File::open(&final_parent)
            .with_context(|| format!("failed to open {}", final_parent.display()))?
            .sync_all()
            .with_context(|| format!("failed to sync dir {}", final_parent.display()))?;
        Ok(())
    })
    .await
    .context("atomic write task failed")??;

    Ok(())
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persists_onboarding_state() {
        let root = std::env::temp_dir().join(format!("home-edge-test-{}", now_unix_ms()));
        let storage = Storage::new(root.clone()).await.expect("storage init");
        let state = OnboardingState {
            version: 1,
            onboarded: true,
            updated_at_unix_ms: now_unix_ms(),
            done: vec!["user".into(), "core_config".into()],
            user: Some(StoredUser {
                name: "Test User".into(),
                username: "test-user".into(),
                password: "test-pass".into(),
                language: "en".into(),
            }),
            location_name: Some("Test Home".into()),
            country: Some("US".into()),
            language: Some("en".into()),
            time_zone: Some("UTC".into()),
            unit_system: Some("metric".into()),
        };

        storage.save_onboarding(&state).await.expect("save state");
        let loaded = storage.load_onboarding().await.expect("load state");

        assert_eq!(loaded.onboarded, state.onboarded);
    }

    #[tokio::test]
    async fn persists_instance_id() {
        let root = std::env::temp_dir().join(format!("home-edge-test-{}", now_unix_ms()));
        let storage = Storage::new(root.clone()).await.expect("storage init");

        let first = storage
            .load_or_create_instance_id()
            .await
            .expect("create instance id");
        let second = storage
            .load_or_create_instance_id()
            .await
            .expect("load instance id");

        assert_eq!(first, second);
    }
}
