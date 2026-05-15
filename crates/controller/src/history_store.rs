//! History store — numeric sensor readings with SQLite persistence.
//!
//! Architecture:
//! - An in-memory ring buffer per entity provides O(1) sparkline reads.
//! - A SQLite database persists all readings across restarts.
//! - On startup, the ring buffer is warmed from the last `capacity` entries per entity.
//! - `record()` writes to both in-memory and SQLite (via spawn_blocking).
//! - `last_n()` uses the in-memory ring buffer (fast — for sparklines).
//! - `since()` queries SQLite directly (for the history page chart).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Serialize;
use tokio::sync::RwLock;

/// A single timestamped sensor reading.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    /// Unix timestamp in seconds.
    pub ts: u64,
    /// Sensor value as f64.
    pub value: f64,
}

struct RingBuffer {
    entries: VecDeque<HistoryEntry>,
    capacity: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, entry: HistoryEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    fn last_n(&self, n: usize) -> Vec<HistoryEntry> {
        self.entries
            .iter()
            .rev()
            .take(n)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

type Db = Arc<std::sync::Mutex<rusqlite::Connection>>;

/// Thread-safe history store backed by SQLite for persistence.
///
/// Call `HistoryStore::open()` in production code. Use `HistoryStore::new()`
/// (in-memory only) in unit tests that do not need persistence.
pub struct HistoryStore {
    inner: RwLock<HashMap<String, RingBuffer>>,
    capacity: usize,
    db: Option<Db>,
}

impl HistoryStore {
    /// Create an in-memory-only store (no persistence). Intended for tests.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            capacity,
            db: None,
        }
    }

    /// Open a SQLite-backed store at `db_path`.
    ///
    /// - Creates the history table and index if they don't exist.
    /// - Warms the in-memory ring buffers from the last `capacity` entries per entity.
    /// - Falls back to in-memory-only on any SQLite error (logs a warning).
    pub fn open(capacity: usize, db_path: &Path) -> Self {
        match Self::try_open(capacity, db_path) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!("history: SQLite open failed ({e:#}); falling back to in-memory");
                Self::new(capacity)
            }
        }
    }

    fn try_open(capacity: usize, db_path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| anyhow::anyhow!("open {}: {e}", db_path.display()))?;

        // WAL mode + NORMAL synchronous: reduces write latency on Pi SD card.
        conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous   = NORMAL;
            CREATE TABLE IF NOT EXISTS history (
                entity_id TEXT    NOT NULL,
                ts        INTEGER NOT NULL,
                value     REAL    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS history_entity_ts ON history (entity_id, ts);
        ")?;

        // Warm the in-memory ring buffers.
        // Load the last `capacity` entries per entity, sorted oldest-first.
        let mut inner: HashMap<String, RingBuffer> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT entity_id, ts, value FROM history ORDER BY entity_id, ts",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })?;
            for row in rows {
                let (entity_id, ts, value) = row?;
                let rb = inner
                    .entry(entity_id)
                    .or_insert_with(|| RingBuffer::new(capacity));
                rb.push(HistoryEntry { ts: ts as u64, value });
            }
        }

        Ok(Self {
            inner: RwLock::new(inner),
            capacity,
            db: Some(Arc::new(std::sync::Mutex::new(conn))),
        })
    }

    /// Record a numeric reading.  Updates the in-memory ring buffer and
    /// asynchronously writes to SQLite.
    pub async fn record(&self, entity_id: &str, value: f64) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = HistoryEntry { ts, value };

        // Update in-memory ring buffer.
        {
            let mut map = self.inner.write().await;
            map.entry(entity_id.to_string())
                .or_insert_with(|| RingBuffer::new(self.capacity))
                .push(entry);
        }

        // Write to SQLite in a blocking task.
        if let Some(db) = self.db.clone() {
            let eid = entity_id.to_string();
            tokio::task::spawn_blocking(move || {
                if let Ok(conn) = db.lock() {
                    if let Err(e) = conn.execute(
                        "INSERT INTO history (entity_id, ts, value) VALUES (?1, ?2, ?3)",
                        rusqlite::params![eid, ts as i64, value],
                    ) {
                        tracing::warn!("history: db write failed: {e}");
                    }
                }
            });
        }
    }

    /// Return the most recent `n` entries for the given entity, oldest first.
    /// Reads from the in-memory ring buffer — use this for sparklines.
    pub async fn last_n(&self, entity_id: &str, n: usize) -> Vec<HistoryEntry> {
        self.inner
            .read()
            .await
            .get(entity_id)
            .map(|rb| rb.last_n(n))
            .unwrap_or_default()
    }

    /// Return all entries for the given entity at or after `since_ts` (unix
    /// seconds), oldest first.  Queries SQLite when available; falls back to
    /// the in-memory buffer when running without a database.
    pub async fn since(&self, entity_id: &str, since_ts: u64) -> Vec<HistoryEntry> {
        if let Some(db) = self.db.clone() {
            let eid = entity_id.to_string();
            let result = tokio::task::spawn_blocking(move || -> Result<Vec<HistoryEntry>> {
                let conn = db.lock().map_err(|_| anyhow::anyhow!("mutex poisoned"))?;
                let mut stmt = conn.prepare(
                    "SELECT ts, value FROM history
                      WHERE entity_id = ?1 AND ts >= ?2
                      ORDER BY ts",
                )?;
                let rows = stmt.query_map(rusqlite::params![eid, since_ts as i64], |row| {
                    Ok(HistoryEntry {
                        ts: row.get::<_, i64>(0)? as u64,
                        value: row.get::<_, f64>(1)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
            })
            .await;
            match result {
                Ok(Ok(entries)) => return entries,
                Ok(Err(e)) => tracing::warn!("history: db query failed: {e}"),
                Err(e) => tracing::warn!("history: spawn_blocking failed: {e}"),
            }
        }
        // Fall back to in-memory buffer.
        self.inner
            .read()
            .await
            .get(entity_id)
            .map(|rb| {
                rb.entries
                    .iter()
                    .filter(|e| e.ts >= since_ts)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return all entity IDs that have at least one history reading recorded
    /// since `since_ts`.  Used to build the history-page entity picker.
    pub async fn entity_ids_since(&self, since_ts: u64) -> Vec<String> {
        if let Some(db) = self.db.clone() {
            let result = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
                let conn = db.lock().map_err(|_| anyhow::anyhow!("mutex poisoned"))?;
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT entity_id FROM history WHERE ts >= ?1 ORDER BY entity_id",
                )?;
                let rows =
                    stmt.query_map(rusqlite::params![since_ts as i64], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
            })
            .await;
            if let Ok(Ok(ids)) = result {
                return ids;
            }
        }
        // Fall back: return all entity IDs from the ring buffer map.
        self.inner.read().await.keys().cloned().collect()
    }
}

/// Render a simple inline `<svg>` sparkline from history entries.
///
/// Returns an SVG string suitable for embedding directly in HTML (`| safe` in
/// templates).
pub fn render_sparkline(entries: &[HistoryEntry], width: u32, height: u32) -> String {
    if entries.len() < 2 {
        return format!(
            "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
             xmlns=\"http://www.w3.org/2000/svg\">\
             <text x=\"50%\" y=\"50%\" text-anchor=\"middle\" dominant-baseline=\"middle\" \
             fill=\"#aaa\" font-size=\"11\">No data yet</text></svg>"
        );
    }

    let values: Vec<f64> = entries.iter().map(|e| e.value).collect();
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = if (max - min).abs() < 1e-9 { 1.0 } else { max - min };

    let w = width as f64;
    let h = height as f64;
    let pad = 4.0_f64;
    let usable_w = w - 2.0 * pad;
    let usable_h = h - 2.0 * pad;

    let n = values.len();
    let points: Vec<(f64, f64)> = values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = pad + (i as f64 / (n - 1) as f64) * usable_w;
            let y = pad + (1.0 - (v - min) / range) * usable_h;
            (x, y)
        })
        .collect();

    let path_d = {
        let mut d = String::new();
        for (i, (x, y)) in points.iter().enumerate() {
            if i == 0 {
                d.push_str(&format!("M {x:.1} {y:.1}"));
            } else {
                d.push_str(&format!(" L {x:.1} {y:.1}"));
            }
        }
        d
    };

    let last = points.last().unwrap();
    let first = points.first().unwrap();
    let area_d = format!(
        "{path_d} L {:.1} {:.1} L {:.1} {:.1} Z",
        last.0,
        h - pad,
        first.0,
        h - pad
    );

    format!(
        "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
         xmlns=\"http://www.w3.org/2000/svg\" style=\"display:block\">\n  \
         <defs>\n    \
         <linearGradient id=\"sg_{width}_{height}\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\n      \
         <stop offset=\"0%\" stop-color=\"#18BCF2\" stop-opacity=\"0.35\"/>\n      \
         <stop offset=\"100%\" stop-color=\"#18BCF2\" stop-opacity=\"0.02\"/>\n    \
         </linearGradient>\n  \
         </defs>\n  \
         <path d=\"{area_d}\" fill=\"url(#sg_{width}_{height})\"/>\n  \
         <path d=\"{path_d}\" fill=\"none\" stroke=\"#18BCF2\" stroke-width=\"2\" \
         stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n\
         </svg>"
    )
}
