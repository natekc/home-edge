//! In-memory entity state store.
//!
//! This is a thin thread-safe map of entity_id → State.  It is the runtime
//! analogue of Home Assistant's `StateMachine` (homeassistant/core.py).
//!
//! API compatibility requirements from homeassistant/core.py:
//! - entity_id must match VALID_ENTITY_ID regex
//! - state string max length: MAX_LENGTH_STATE_STATE = 255
//! - all states have a Context
//! - last_changed / last_updated / last_reported are always ISO 8601

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use ha_types::context::Context;
use ha_types::entity::State;
use tokio::sync::broadcast;
use uuid::Uuid;

/// A state change event broadcast when a state is inserted or updated.
/// Source: homeassistant/core.py  Event(EVENT_STATE_CHANGED)
#[derive(Clone, Debug)]
pub struct StateEvent {
    pub state: State,
    pub old_state: Option<State>,
}

/// Thread-safe in-memory entity state store.
pub struct StateStore {
    states: RwLock<HashMap<String, State>>,
    change_tx: broadcast::Sender<StateEvent>,
}

impl StateStore {
    pub fn new() -> Self {
        let (change_tx, _) = broadcast::channel(256);
        Self {
            states: RwLock::new(HashMap::new()),
            change_tx,
        }
    }

    /// Return all current states, order is unspecified (mirrors HA behaviour).
    pub fn all(&self) -> Vec<State> {
        let lock = self.states.read().expect("state lock poisoned");
        lock.values().cloned().collect()
    }

    /// Return a single state by entity_id, or None.
    pub fn get(&self, entity_id: &str) -> Option<State> {
        let lock = self.states.read().expect("state lock poisoned");
        lock.get(entity_id).cloned()
    }

    /// Insert or replace a state entry.
    ///
    /// Returns Err if the entity_id is invalid.
    /// Source: homeassistant/core.py  StateMachine.async_set / valid_entity_id
    pub fn set(&self, state: State) -> Result<(), String> {
        if !State::is_valid_entity_id(&state.entity_id) {
            return Err(format!("Invalid entity ID: {}", state.entity_id));
        }
        if state.state.len() > 255 {
            return Err("State value exceeds maximum length of 255".into());
        }
        let old_state = {
            let mut lock = self.states.write().expect("state lock poisoned");
            let old = lock.get(&state.entity_id).cloned();
            lock.insert(state.entity_id.clone(), state.clone());
            old
        };
        let _ = self.change_tx.send(StateEvent { state, old_state });
        Ok(())
    }

    /// Subscribe to state change events.
    pub fn subscribe(&self) -> broadcast::Receiver<StateEvent> {
        self.change_tx.subscribe()
    }

    /// Remove a state. Returns true if it existed.
    #[cfg(test)]
    pub fn remove(&self, entity_id: &str) -> bool {
        let mut lock = self.states.write().expect("state lock poisoned");
        lock.remove(entity_id).is_some()
    }
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

pub fn now_iso8601() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Format: "2026-01-01T12:00:00.000000+00:00"
    // We use a simple implementation that matches HA's isoformat() output.
    let secs = now.as_secs();
    let micros = now.subsec_micros();
    let (y, mo, d, h, mi, s) = epoch_to_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{micros:06}+00:00")
}

fn epoch_to_parts(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let total_min = secs / 60;
    let mi = total_min % 60;
    let total_hours = total_min / 60;
    let h = total_hours % 24;
    let total_days = total_hours / 24;

    // Gregorian calendar calculation
    let (y, mo, d) = days_to_ymd(total_days);
    (y, mo, d, h, mi, s)
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Days since Unix epoch (1970-01-01)
    let mut y = 1970u64;
    let mut remaining = days;

    loop {
        let leap = is_leap(y);
        let days_in_year = if leap { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }

    let leap = is_leap(y);
    let months = [
        31u64,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1u64;
    for &days_in_month in &months {
        if remaining < days_in_month {
            break;
        }
        remaining -= days_in_month;
        mo += 1;
    }

    (y, mo, remaining + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Typed attribute container that bridges the core domain and the HA wire format.
///
/// Adapters parse their JSON payloads into `StateAttributes` before passing
/// them into `OperationRequest` or `make_state*`. The raw `HashMap` is only
/// materialised at the store boundary, keeping `serde_json` types out of the
/// core operation types.
#[derive(Clone, Debug, Default)]
pub struct StateAttributes(HashMap<String, serde_json::Value>);

impl StateAttributes {
    /// Empty attribute set.
    pub fn empty() -> Self {
        Self(HashMap::new())
    }

    /// Build from a JSON object map (called at the REST/WS transport edge).
    pub fn from_json_object(obj: &serde_json::Map<String, serde_json::Value>) -> Self {
        Self(obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    /// Wrap an already-built `HashMap` (used by service dispatch internally).
    pub(crate) fn from_hash(map: HashMap<String, serde_json::Value>) -> Self {
        Self(map)
    }

    /// Consume into the raw map required by `ha_types::entity::State`.
    pub(crate) fn into_inner(self) -> HashMap<String, serde_json::Value> {
        self.0
    }
}

/// Build a new State with current timestamps and a generated context.
pub fn make_state(
    entity_id: impl Into<String>,
    state_value: impl Into<String>,
    attributes: StateAttributes,
) -> State {
    make_state_with_context(
        entity_id,
        state_value,
        attributes,
        Context::new(new_context_id()),
    )
}

pub fn make_state_with_context(
    entity_id: impl Into<String>,
    state_value: impl Into<String>,
    attributes: StateAttributes,
    context: Context,
) -> State {
    let ts = now_iso8601();
    State {
        entity_id: entity_id.into(),
        state: state_value.into(),
        attributes: attributes.into_inner(),
        last_changed: ts.clone(),
        last_reported: ts.clone(),
        last_updated: ts,
        context,
    }
}

fn new_context_id() -> String {
    Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state(entity_id: &str) -> State {
        make_state(entity_id, "on", StateAttributes::empty())
    }

    #[test]
    fn empty_store_returns_no_states() {
        let store = StateStore::new();
        assert!(store.all().is_empty());
    }

    #[test]
    fn set_and_get_state() {
        let store = StateStore::new();
        store.set(sample_state("light.living_room")).unwrap();
        let s = store.get("light.living_room").unwrap();
        assert_eq!(s.entity_id, "light.living_room");
        assert_eq!(s.state, "on");
    }

    #[test]
    fn all_returns_all_states() {
        let store = StateStore::new();
        store.set(sample_state("light.a")).unwrap();
        store.set(sample_state("light.b")).unwrap();
        assert_eq!(store.all().len(), 2);
    }

    #[test]
    fn remove_state() {
        let store = StateStore::new();
        store.set(sample_state("light.a")).unwrap();
        assert!(store.remove("light.a"));
        assert!(store.get("light.a").is_none());
        assert!(!store.remove("light.a")); // already gone
    }

    /// Source: homeassistant/core.py  valid_entity_id check in StateMachine.async_set
    #[test]
    fn rejects_invalid_entity_id() {
        let store = StateStore::new();
        let bad = State {
            entity_id: "no_dot".into(),
            state: "on".into(),
            attributes: HashMap::new(),
            last_changed: "".into(),
            last_reported: "".into(),
            last_updated: "".into(),
            context: ha_types::context::Context::new("x"),
        };
        assert!(store.set(bad).is_err());
    }

    /// Source: homeassistant/core.py  MAX_LENGTH_STATE_STATE = 255
    #[test]
    fn rejects_state_too_long() {
        let store = StateStore::new();
        let long_state = "x".repeat(256);
        let s = State {
            entity_id: "sensor.test".into(),
            state: long_state,
            attributes: HashMap::new(),
            last_changed: "".into(),
            last_reported: "".into(),
            last_updated: "".into(),
            context: ha_types::context::Context::new("x"),
        };
        assert!(store.set(s).is_err());
    }

    #[test]
    fn now_iso8601_has_correct_format() {
        let ts = now_iso8601();
        // Expected: "YYYY-MM-DDTHH:MM:SS.mmmmmmm+00:00"
        assert!(ts.ends_with("+00:00"), "must end with +00:00: {ts}");
        assert!(ts.contains('T'), "must contain T separator: {ts}");
        assert_eq!(ts.len(), 32, "must be 32 chars: {ts}");
    }
}
