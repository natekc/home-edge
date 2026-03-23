use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::storage::{Storage, StoredUser, save_json_atomic};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthUser {
    pub name: String,
    pub username: String,
    pub password: String,
    pub language: String,
}

pub struct AuthStore {
    root: PathBuf,
    lock: Mutex<()>,
}

impl AuthStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
        }
    }

    pub async fn load_user(&self) -> Result<Option<AuthUser>> {
        let path = self.user_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                let user = serde_json::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                Ok(Some(user))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub async fn save_user(&self, user: &AuthUser) -> Result<()> {
        let _guard = self.lock.lock().await;
        save_json_atomic(&self.user_path(), user).await
    }

    pub async fn load_user_with_legacy_fallback(
        &self,
        storage: &Storage,
    ) -> Result<Option<AuthUser>> {
        if let Some(user) = self.load_user().await? {
            return Ok(Some(user));
        }

        let onboarding = storage.load_onboarding().await?;
        let Some(legacy_user) = onboarding.user else {
            return Ok(None);
        };

        let user = AuthUser::from(legacy_user);
        self.save_user(&user).await?;
        Ok(Some(user))
    }

    fn user_path(&self) -> PathBuf {
        self.root.join("auth_user.json")
    }
}

impl From<StoredUser> for AuthUser {
    fn from(value: StoredUser) -> Self {
        Self {
            name: value.name,
            username: value.username,
            password: value.password,
            language: value.language,
        }
    }
}

impl From<&StoredUser> for AuthUser {
    fn from(value: &StoredUser) -> Self {
        Self {
            name: value.name.clone(),
            username: value.username.clone(),
            password: value.password.clone(),
            language: value.language.clone(),
        }
    }
}

impl From<&AuthUser> for StoredUser {
    fn from(value: &AuthUser) -> Self {
        Self {
            name: value.name.clone(),
            username: value.username.clone(),
            password: value.password.clone(),
            language: value.language.clone(),
        }
    }
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

    #[tokio::test]
    async fn persists_auth_user() {
        let root = temp_dir("auth-store");
        let store = AuthStore::new(root);
        let user = AuthUser {
            name: "Test".into(),
            username: "test-user".into(),
            password: "secret".into(),
            language: "en".into(),
        };

        store.save_user(&user).await.expect("save auth user");
        let loaded = store.load_user().await.expect("load auth user");

        assert_eq!(loaded, Some(user));
    }

    #[tokio::test]
    async fn migrates_legacy_onboarding_user() {
        let root = temp_dir("auth-store-legacy");
        let storage = Storage::new(root.clone()).await.expect("storage");
        storage
            .save_onboarding(&crate::storage::OnboardingState {
                user: Some(StoredUser {
                    name: "Legacy".into(),
                    username: "legacy-user".into(),
                    password: "secret".into(),
                    language: "en".into(),
                }),
                ..Default::default()
            })
            .await
            .expect("save onboarding");

        let store = AuthStore::new(root);
        let loaded = store
            .load_user_with_legacy_fallback(&storage)
            .await
            .expect("load or migrate user");

        assert_eq!(loaded.expect("user").username, "legacy-user");
        assert!(store.load_user().await.expect("load user").is_some());
    }
}
