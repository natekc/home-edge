//! Ring-buffer of recent entity state changes for the Logbook page.
//!
//! Source: homeassistant/components/logbook/processor.py state_change rows

use std::collections::VecDeque;

use serde::Serialize;
use tokio::sync::RwLock;

/// A single recorded state transition.
#[derive(Debug, Clone, Serialize)]
pub struct LogbookEntry {
    /// Unix timestamp in seconds.
    pub ts: u64,
    pub entity_id: String,
    /// `friendly_name` attribute, or `entity_id` if absent.
    pub display_name: String,
    /// Previous state value; empty string if first-seen.
    pub old_state: String,
    pub new_state: String,
}

/// Thread-safe in-memory ring-buffer for logbook entries.
pub struct LogbookStore {
    inner: RwLock<VecDeque<LogbookEntry>>,
    capacity: usize,
}

impl LogbookStore {
    /// Create a new logbook store with the given total ring-buffer capacity.
    ///
    /// `capacity` is typically sourced from `AppConfig.history.capacity`.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Append an entry, evicting the oldest if at capacity.
    pub async fn record(&self, entry: LogbookEntry) {
        if self.capacity == 0 {
            return;
        }
        let mut buf = self.inner.write().await;
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(entry);
    }

    /// Return the most recent `n` entries, newest first.
    pub async fn recent_n(&self, n: usize) -> Vec<LogbookEntry> {
        let buf = self.inner.read().await;
        buf.iter().rev().take(n).cloned().collect()
    }

    /// Return up to `limit` entries, newest first, optionally filtered by entity_id.
    ///
    /// When `entity_id_filter` is `Some`, only entries whose `entity_id` contains
    /// the filter string (case-insensitive substring match) are returned.
    ///
    /// Source: homeassistant/components/logbook/__init__.py entity_id filter
    pub async fn get_filtered(
        &self,
        entity_id_filter: Option<&str>,
        limit: usize,
    ) -> Vec<LogbookEntry> {
        let buf = self.inner.read().await;
        buf.iter()
            .rev()
            .filter(|e| {
                entity_id_filter
                    .map(|f| e.entity_id.to_lowercase().contains(&f.to_lowercase()))
                    .unwrap_or(true)
            })
            .take(limit)
            .cloned()
            .collect()
    }
}
