//! Long-lived access token store.
//!
//! Source: homeassistant/auth/auth_store.py  RefreshToken
//!         homeassistant/components/auth/__init__.py  CreateTokenView
//!
//! Tokens are persisted as a JSON file in the home-edge data directory.
//! Each record stores a SHA-256 hex digest of the plaintext token so the
//! plaintext is never held on disk. The token is returned once on creation
//! and must be copied immediately; it cannot be recovered later.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use uuid::Uuid;

const TOKENS_FILE: &str = "long_lived_tokens.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub id: String,
    pub name: String,
    /// SHA-256 hex digest of the plaintext token — never the token itself.
    token_hash: String,
    pub created_at: String,
}

pub struct LongLivedTokenStore {
    root: PathBuf,
    inner: RwLock<Vec<TokenRecord>>,
}

impl LongLivedTokenStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            inner: RwLock::new(vec![]),
        }
    }

    /// Load persisted tokens from disk into the in-memory store.
    /// Call once at startup (mirroring `TokenStore::load_persisted`).
    pub async fn load(&self) -> Result<()> {
        let path = self.root.join(TOKENS_FILE);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => {
                let records: Vec<TokenRecord> = serde_json::from_str(&s).unwrap_or_default();
                *self.inner.write().await = records;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn persist(&self) -> Result<()> {
        let records = self.inner.read().await;
        let json = serde_json::to_string_pretty(&*records)?;
        let path = self.root.join(TOKENS_FILE);
        tokio::fs::write(&path, json.as_bytes()).await?;
        Ok(())
    }

    /// Return all token summaries (id, name, created_at — no hash exposed).
    pub async fn list(&self) -> Vec<TokenSummary> {
        self.inner
            .read()
            .await
            .iter()
            .map(|r| TokenSummary {
                id: r.id.clone(),
                name: r.name.clone(),
                created_at: r.created_at.clone(),
            })
            .collect()
    }

    /// Create a new token with `name`. Returns the 64-char plaintext token
    /// (shown once; caller must present it to the user immediately).
    ///
    /// Source: homeassistant/components/auth/__init__.py  CreateTokenView
    pub async fn create(&self, name: String) -> Result<String> {
        // 64-char random token: two UUID v4 simple strings (no hyphens).
        let token = format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let token_hash = sha256_hex(token.as_bytes());
        let id = Uuid::new_v4().to_string();
        let created_at = format_today();
        let record = TokenRecord { id, name, token_hash, created_at };
        self.inner.write().await.push(record);
        self.persist().await?;
        Ok(token)
    }

    /// Revoke a token by `id`. Returns `true` if a record was removed.
    pub async fn revoke(&self, id: &str) -> Result<bool> {
        let mut inner = self.inner.write().await;
        let before = inner.len();
        inner.retain(|r| r.id != id);
        let removed = inner.len() < before;
        drop(inner);
        if removed {
            self.persist().await?;
        }
        Ok(removed)
    }

    /// Verify a bearer token against stored SHA-256 hashes. O(n) where n is
    /// the number of tokens — always tiny for a home hub.
    pub async fn verify_bearer(&self, token: &str) -> bool {
        let hash = sha256_hex(token.as_bytes());
        self.inner.read().await.iter().any(|r| r.token_hash == hash)
    }
}

/// Summary view of a token record safe to pass to templates (no hash).
#[derive(Debug, Clone, Serialize)]
pub struct TokenSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

/// Compute SHA-256 and return a lowercase hex string.
fn sha256_hex(input: &[u8]) -> String {
    let digest = Sha256::digest(input);
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Return today's date as "YYYY-MM-DD" from the system clock.
/// Uses the proleptic Gregorian calendar algorithm — no chrono dependency.
///
/// Source: Howard Hinnant's civil_from_days algorithm
/// <http://howardhinnant.github.io/date_algorithms.html>
fn format_today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    days_to_ymd(secs / 86400)
}

fn days_to_ymd(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
