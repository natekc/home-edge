use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub notification_id: String,
    pub title: Option<String>,
    pub message: String,
    pub created_at: u64,
    pub status: String,
}

pub struct NotificationStore {
    inner: RwLock<HashMap<String, Notification>>,
}

impl NotificationStore {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    pub async fn create(
        &self,
        message: String,
        title: Option<String>,
        notification_id: Option<String>,
    ) -> Notification {
        let id = notification_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let notif = Notification {
            notification_id: id.clone(),
            title,
            message,
            created_at: ts,
            status: "unread".to_string(),
        };
        self.inner.write().await.insert(id, notif.clone());
        notif
    }

    pub async fn dismiss(&self, notification_id: &str) -> bool {
        self.inner.write().await.remove(notification_id).is_some()
    }

    /// Return all notifications, sorted newest first.
    pub async fn all(&self) -> Vec<Notification> {
        let mut list: Vec<Notification> =
            self.inner.read().await.values().cloned().collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        list
    }
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self::new()
    }
}
