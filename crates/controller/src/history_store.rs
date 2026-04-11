//! In-memory ring-buffer history store for numeric sensor values.
//!
//! Each entity gets a fixed-capacity circular buffer.  Values are stored as
//! `f64`; non-numeric sensor states (e.g. "on", "off", "unavailable") are
//! silently ignored by the caller before reaching this store.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Thread-safe in-memory history store.
pub struct HistoryStore {
    inner: RwLock<HashMap<String, RingBuffer>>,
    capacity: usize,
}

impl HistoryStore {
    /// Create a new history store with the given per-entity ring-buffer capacity.
    ///
    /// `capacity` is typically sourced from `AppConfig.history.capacity`.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            capacity,
        }
    }

    /// Record a numeric reading for the given entity_id.
    pub async fn record(&self, entity_id: &str, value: f64) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = HistoryEntry { ts, value };
        let mut map = self.inner.write().await;
        map.entry(entity_id.to_string())
            .or_insert_with(|| RingBuffer::new(self.capacity))
            .push(entry);
    }

    /// Return the most recent `n` entries for the given entity, oldest first.
    pub async fn last_n(&self, entity_id: &str, n: usize) -> Vec<HistoryEntry> {
        self.inner
            .read()
            .await
            .get(entity_id)
            .map(|rb| rb.last_n(n))
            .unwrap_or_default()
    }
}

/// Render a simple inline `<svg>` sparkline from history entries.
///
/// Returns an SVG string suitable for embedding directly in HTML (no `| safe`
/// filter needed — the caller must mark it safe or embed it as raw HTML).
pub fn render_sparkline(entries: &[HistoryEntry], width: u32, height: u32) -> String {
    if entries.len() < 2 {
        return format!(
            "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" xmlns=\"http://www.w3.org/2000/svg\"><text x=\"50%\" y=\"50%\" text-anchor=\"middle\" dominant-baseline=\"middle\" fill=\"#aaa\" font-size=\"11\">No data yet</text></svg>"
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
        "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" xmlns=\"http://www.w3.org/2000/svg\" style=\"display:block\">\n  \
         <defs>\n    \
         <linearGradient id=\"sg_{width}_{height}\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\n      \
         <stop offset=\"0%\" stop-color=\"#18BCF2\" stop-opacity=\"0.35\"/>\n      \
         <stop offset=\"100%\" stop-color=\"#18BCF2\" stop-opacity=\"0.02\"/>\n    \
         </linearGradient>\n  \
         </defs>\n  \
         <path d=\"{area_d}\" fill=\"url(#sg_{width}_{height})\"/>\n  \
         <path d=\"{path_d}\" fill=\"none\" stroke=\"#18BCF2\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n\
         </svg>"
    )
}
