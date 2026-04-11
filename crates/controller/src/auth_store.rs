use std::path::PathBuf;

use anyhow::{Context, Result};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use rand_core::OsRng;
use tokio::sync::Mutex;

use crate::storage::{Storage, StoredUser, save_json_atomic};

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

    pub async fn load_user(&self) -> Result<Option<StoredUser>> {
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

    pub async fn save_user(&self, user: &StoredUser) -> Result<()> {
        let _guard = self.lock.lock().await;
        save_json_atomic(&self.user_path(), user).await
    }

    pub async fn load_user_with_legacy_fallback(
        &self,
        storage: &Storage,
    ) -> Result<Option<StoredUser>> {
        if let Some(user) = self.load_user().await? {
            return Ok(Some(user));
        }

        let onboarding = storage.load_onboarding().await?;
        let Some(legacy_user) = onboarding.user else {
            return Ok(None);
        };

        self.save_user(&legacy_user).await?;
        Ok(Some(legacy_user))
    }

    fn user_path(&self) -> PathBuf {
        self.root.join("auth_user.json")
    }
}

/// Hash a password with Argon2id and return the PHC string.
///
/// CPU-bound; call from a spawned blocking task in async contexts where
/// latency matters (login), but suitable inline for one-time onboarding.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))?
        .to_string();
    Ok(hash)
}

/// Verify a candidate password against a stored value.
///
/// Handles two cases:
/// - PHC string (`$argon2id$…`) — constant-time argon2 verification.
/// - Legacy plaintext — plain equality check used only during the migration
///   window before the user's first post-upgrade login.
///
/// Returns `true` if the candidate is correct.
pub fn verify_password(candidate: &str, stored: &str) -> bool {
    if stored.starts_with("$argon2") {
        match PasswordHash::new(stored) {
            Ok(parsed) => Argon2::default()
                .verify_password(candidate.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    } else {
        // Legacy plaintext — timing is acceptable here since we immediately
        // re-hash and save on the first successful login (see ha_auth.rs).
        candidate == stored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::temp_dir;

    #[test]
    fn hash_and_verify_round_trip() {
        let hashed = hash_password("correct-horse").expect("hash");
        assert!(hashed.starts_with("$argon2"), "must be PHC string");
        assert!(verify_password("correct-horse", &hashed), "correct password must verify");
        assert!(!verify_password("wrong-password", &hashed), "wrong password must not verify");
    }

    #[test]
    fn verify_legacy_plaintext_falls_back_correctly() {
        // Simulates a user record that was stored before the hashing migration.
        assert!(verify_password("secret", "secret"), "plaintext match");
        assert!(!verify_password("other", "secret"), "plaintext mismatch");
    }

    #[tokio::test]
    async fn persists_auth_user() {
        let root = temp_dir("auth-store");
        let store = AuthStore::new(root);
        let hashed = hash_password("secret").expect("hash password");
        let user = StoredUser {
            name: "Test".into(),
            username: "test-user".into(),
            password: hashed,
            language: "en".into(),
        };

        store.save_user(&user).await.expect("save auth user");
        let loaded = store.load_user().await.expect("load auth user").expect("user");

        assert!(loaded.password.starts_with("$argon2"), "stored password must be hashed");
        assert_eq!(loaded.username, "test-user");
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
