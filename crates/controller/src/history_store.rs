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

    /// Return the most recent value for every entity that has history.
    ///
    /// Used at startup to re-seed the in-memory `StateStore` with the last
    /// known sensor readings so entities don't show "unavailable" after a
    /// redeploy until the sensor next reports.
    ///
    /// Queries SQLite when available; falls back to the in-memory ring buffer.
    pub async fn latest_values(&self) -> Vec<(String, f64)> {
        if let Some(db) = self.db.clone() {
            let result = tokio::task::spawn_blocking(move || -> Result<Vec<(String, f64)>> {
                let conn = db.lock().map_err(|_| anyhow::anyhow!("mutex poisoned"))?;
                // One row per entity: the reading with the highest ts.
                let mut stmt = conn.prepare(
                    "SELECT entity_id, value FROM history
                      WHERE ts = (SELECT MAX(ts) FROM history h2 WHERE h2.entity_id = history.entity_id)
                      GROUP BY entity_id",
                )?;
                let rows = stmt.query_map(rusqlite::params![], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
            })
            .await;
            if let Ok(Ok(pairs)) = result {
                return pairs;
            }
        }
        // Fall back: last entry from each in-memory ring buffer.
        self.inner
            .read()
            .await
            .iter()
            .filter_map(|(eid, rb)| rb.entries.back().map(|e| (eid.clone(), e.value)))
            .collect()
    }
}

/// Format a unix timestamp as HH:MM (UTC wall-clock, no timezone deps).
fn format_hhmm(unix_ts: u64) -> String {
    let secs_in_day = unix_ts % 86400;
    let h = secs_in_day / 3600;
    let m = (secs_in_day % 3600) / 60;
    format!("{h:02}:{m:02}")
}

/// Render a full 400×120 history line chart SVG with axis labels.
///
/// Padding: 20px left (Y-axis space), 8px right/top, 20px bottom (X-axis space).
/// Returns an SVG string safe to embed via `| safe` in Minijinja templates.
pub fn render_history_chart(entries: &[HistoryEntry], width: u32, height: u32) -> String {
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
    let pad_l = 20.0_f64;
    let pad_r = 8.0_f64;
    let pad_t = 8.0_f64;
    let pad_b = 20.0_f64;
    let usable_w = w - pad_l - pad_r;
    let usable_h = h - pad_t - pad_b;

    let n = values.len();
    let points: Vec<(f64, f64)> = values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = pad_l + (i as f64 / (n - 1) as f64) * usable_w;
            let y = pad_t + (1.0 - (v - min) / range) * usable_h;
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
        last.0, h - pad_b,
        first.0, h - pad_b,
    );

    // Y-axis tick labels: min, mid, max
    let mid = (min + max) / 2.0;
    let y_max_px = pad_t;
    let y_mid_px = pad_t + usable_h / 2.0;
    let y_min_px = pad_t + usable_h;

    // X-axis labels: oldest and newest timestamps as HH:MM
    let ts_first = entries.first().unwrap().ts;
    let ts_last  = entries.last().unwrap().ts;
    let x_first_label = format_hhmm(ts_first);
    let x_last_label  = format_hhmm(ts_last);

    format!(
        "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
         xmlns=\"http://www.w3.org/2000/svg\" style=\"display:block\">\n  \
         <defs>\n    \
         <linearGradient id=\"hcg_{width}_{height}\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\n      \
         <stop offset=\"0%\" stop-color=\"#18BCF2\" stop-opacity=\"0.35\"/>\n      \
         <stop offset=\"100%\" stop-color=\"#18BCF2\" stop-opacity=\"0.02\"/>\n    \
         </linearGradient>\n  \
         </defs>\n  \
         <path d=\"{area_d}\" fill=\"url(#hcg_{width}_{height})\"/>\n  \
         <path d=\"{path_d}\" fill=\"none\" stroke=\"#18BCF2\" stroke-width=\"2\" \
         stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n  \
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"#999\" font-size=\"9\">{:.1}</text>\n  \
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"#999\" font-size=\"9\">{:.1}</text>\n  \
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"#999\" font-size=\"9\">{:.1}</text>\n  \
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"start\" fill=\"#999\" font-size=\"9\">{x_first_label}</text>\n  \
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"#999\" font-size=\"9\">{x_last_label}</text>\n\
         </svg>",
        // Y-axis labels (x, y, value)
        pad_l - 2.0, y_max_px + 9.0, max,
        pad_l - 2.0, y_mid_px + 4.0, mid,
        pad_l - 2.0, y_min_px,       min,
        // X-axis labels (x, y)
        pad_l,       h,
        w - pad_r,   h,
    )
}

/// Render a horizontal binary timeline SVG (400×28 by default).
///
/// Each segment is colored accent (`#18BCF2`) when on (value ≥ 0.5) or
/// neutral gray (`#9E9E9E`) when off.  The timeline spans from `since_ts`
/// to `since_ts + 86400` (one 24 h window).
///
/// Returns an SVG string safe to embed via `| safe` in Minijinja templates.
pub fn render_binary_timeline(
    entries: &[HistoryEntry],
    width: u32,
    height: u32,
    since_ts: u64,
) -> String {
    let end_ts = since_ts + 86400;
    let span = (end_ts - since_ts) as f64;
    let w = width as f64;
    let h = height as f64;

    if entries.is_empty() {
        return format!(
            "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
             xmlns=\"http://www.w3.org/2000/svg\">\
             <text x=\"50%\" y=\"50%\" text-anchor=\"middle\" dominant-baseline=\"middle\" \
             fill=\"#aaa\" font-size=\"11\">No history yet</text></svg>"
        );
    }

    let mut rects = String::new();
    for i in 0..entries.len() {
        let seg_start = entries[i].ts.max(since_ts);
        let seg_end = if i + 1 < entries.len() {
            entries[i + 1].ts.min(end_ts)
        } else {
            end_ts
        };
        if seg_end <= seg_start {
            continue;
        }
        let x = ((seg_start - since_ts) as f64 / span) * w;
        let seg_w = ((seg_end - seg_start) as f64 / span) * w;
        let color = if entries[i].value >= 0.5 { "#18BCF2" } else { "#9E9E9E" };
        rects.push_str(&format!(
            "<rect x=\"{x:.1}\" y=\"0\" width=\"{seg_w:.1}\" height=\"{h}\" fill=\"{color}\"/>\n  "
        ));
    }

    format!(
        "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
         xmlns=\"http://www.w3.org/2000/svg\" style=\"display:block;border-radius:4px;overflow:hidden\">\n  \
         {rects}</svg>"
    )
}

/// Compute min / mean / max over the value field of `entries`.
///
/// Returns `None` if `entries` is empty.
pub fn stats_for_period(entries: &[HistoryEntry]) -> Option<(f64, f64, f64)> {
    if entries.is_empty() {
        return None;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut sum = 0.0_f64;
    for e in entries {
        if e.value < min { min = e.value; }
        if e.value > max { max = e.value; }
        sum += e.value;
    }
    let mean = sum / entries.len() as f64;
    Some((min, mean, max))
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

/// Compute min, mean, and max for a slice of history entries.
///
/// Returns `None` if the slice is empty.
///
/// Source: homeassistant/components/recorder/statistics.py StatisticsRow
pub fn stats_for_slice(entries: &[HistoryEntry]) -> Option<(f64, f64, f64)> {
    if entries.is_empty() {
        return None;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut sum = 0.0_f64;
    for e in entries {
        if e.value < min {
            min = e.value;
        }
        if e.value > max {
            max = e.value;
        }
        sum += e.value;
    }
    let mean = sum / entries.len() as f64;
    Some((min, mean, max))
}

/// Render a binary sensor timeline — horizontal colored stripes for on/off periods.
///
/// Entries should have values 1.0 (on) or 0.0 (off), ordered by ts.
/// The timeline spans from `since_ts` to now, with each segment colored by
/// whether the sensor was on (amber) or off (light grey) during that period.
///
/// Source: homeassistant/components/history/ binary sensor visualization
pub fn render_binary_timeline(
    entries: &[HistoryEntry],
    width: u32,
    height: u32,
    since_ts: u64,
) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let span = if now > since_ts { (now - since_ts) as f64 } else { 1.0 };
    let w = width as f64;
    let h = height as f64;

    if entries.is_empty() {
        return format!(
            "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
             xmlns=\"http://www.w3.org/2000/svg\" style=\"display:block\">\
             <rect x=\"0\" y=\"0\" width=\"{width}\" height=\"{height}\" fill=\"#e5e7eb\" rx=\"4\"/>\
             <text x=\"50%\" y=\"50%\" text-anchor=\"middle\" dominant-baseline=\"middle\" \
             fill=\"#aaa\" font-size=\"11\">No data</text></svg>"
        );
    }

    let mut rects = String::new();
    // Build segments: entry[i] holds the value active from entries[i].ts
    // until entries[i+1].ts (or `now` for the last entry).
    for i in 0..entries.len() {
        let seg_start = entries[i].ts.max(since_ts);
        let seg_end = if i + 1 < entries.len() {
            entries[i + 1].ts.min(now)
        } else {
            now
        };
        if seg_end <= seg_start {
            continue;
        }
        let x = ((seg_start - since_ts) as f64 / span * w).max(0.0);
        let x2 = ((seg_end - since_ts) as f64 / span * w).min(w);
        let seg_w = (x2 - x).max(0.5);
        // on = warm amber; off = light grey
        let color = if entries[i].value >= 0.5 { "#f59e0b" } else { "#e5e7eb" };
        rects.push_str(&format!(
            "<rect x=\"{x:.1}\" y=\"0\" width=\"{seg_w:.1}\" height=\"{h:.1}\" fill=\"{color}\"/>"
        ));
    }

    format!(
        "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
         xmlns=\"http://www.w3.org/2000/svg\" style=\"display:block;border-radius:4px\">\
         <rect x=\"0\" y=\"0\" width=\"{width}\" height=\"{height}\" fill=\"#e5e7eb\"/>\
         {rects}\
         </svg>"
    )
}
