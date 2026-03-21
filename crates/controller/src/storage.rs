use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

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
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            version: 1,
            onboarded: false,
            updated_at_unix_ms: now_unix_ms(),
        }
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
    pub fn new_in_memory() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ha-compat-test-{}",
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

    fn onboarding_path(&self) -> PathBuf {
        self.root.join("onboarding.json")
    }
}

async fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
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
        let root = std::env::temp_dir().join(format!("pi-control-plane-test-{}", now_unix_ms()));
        let storage = Storage::new(root.clone()).await.expect("storage init");
        let state = OnboardingState {
            version: 1,
            onboarded: true,
            updated_at_unix_ms: now_unix_ms(),
        };

        storage.save_onboarding(&state).await.expect("save state");
        let loaded = storage.load_onboarding().await.expect("load state");

        assert_eq!(loaded.onboarded, state.onboarded);
    }
}
