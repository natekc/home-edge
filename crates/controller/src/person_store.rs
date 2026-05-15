//! Person entity store — tracks people's locations via device trackers.
//!
//! Source: homeassistant/components/person/__init__.py  PersonStorageCollection

use std::collections::HashMap;
use std::sync::RwLock;

use crate::zone_store::StoredZone;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A person record.
///
/// Source: homeassistant/components/person/__init__.py  STORAGE_FIELDS
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Person {
    /// Stable slug used as entity_id suffix: `person.<id>`.
    pub id: String,
    /// Human-readable display name.
    /// Source: homeassistant/components/person/__init__.py  ATTR_NAME
    pub name: String,
    /// Device tracker webhook_ids whose location updates drive this person.
    /// Source: homeassistant/components/person/__init__.py  CONF_DEVICE_TRACKERS
    pub device_trackers: Vec<String>,
    /// Linked HA user ID (optional).
    /// Source: homeassistant/components/person/__init__.py  CONF_USER_ID
    pub user_id: Option<String>,
}

/// Runtime state for a person entity.
///
/// Source: homeassistant/components/person/__init__.py  PersonState / _async_handle_tracker_update
#[derive(Debug, Clone)]
pub struct PersonState {
    pub person_id: String,
    /// Zone name, "home", "not_home", or "unknown".
    /// Source: homeassistant/components/zone/__init__.py  STATE_HOME / STATE_NOT_HOME
    pub state: String,
    /// Source: homeassistant/helpers/entity.py  ATTR_LATITUDE
    pub latitude: Option<f64>,
    /// Source: homeassistant/helpers/entity.py  ATTR_LONGITUDE
    pub longitude: Option<f64>,
    /// Source: homeassistant/helpers/entity.py  ATTR_GPS_ACCURACY
    pub gps_accuracy: Option<f32>,
    /// Which device_tracker (webhook_id) triggered this update.
    /// Source: homeassistant/components/person/__init__.py  ATTR_SOURCE
    pub source: Option<String>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// In-memory person registry with runtime location states.
pub struct PersonStore {
    persons: RwLock<Vec<Person>>,
    states: RwLock<HashMap<String, PersonState>>,
}

impl PersonStore {
    pub fn new() -> Self {
        Self {
            persons: RwLock::new(vec![]),
            states: RwLock::new(HashMap::new()),
        }
    }

    /// Return all registered persons.
    pub fn all_persons(&self) -> Vec<Person> {
        self.persons.read().unwrap().clone()
    }

    /// Insert or update a person record (matched by `id`).
    pub fn add_or_update(&self, person: Person) {
        let mut lock = self.persons.write().unwrap();
        if let Some(p) = lock.iter_mut().find(|p| p.id == person.id) {
            *p = person;
        } else {
            lock.push(person);
        }
    }

    /// Remove a person by id. Returns true if the person existed.
    pub fn remove(&self, person_id: &str) -> bool {
        let mut lock = self.persons.write().unwrap();
        let before = lock.len();
        lock.retain(|p| p.id != person_id);
        lock.len() < before
    }

    /// Called when a device_tracker reports a new GPS location.
    ///
    /// Updates the state of every person whose `device_trackers` list contains
    /// `device_id`. The new state is the name of the containing zone, or
    /// "not_home" if outside all zones.
    ///
    /// Source: homeassistant/components/person/__init__.py  _async_handle_tracker_update
    pub fn handle_location_update(
        &self,
        device_id: &str,
        lat: f64,
        lon: f64,
        accuracy: f32,
        zones: &[StoredZone],
    ) {
        let persons = self.persons.read().unwrap();
        for person in persons.iter() {
            if !person.device_trackers.contains(&device_id.to_string()) {
                continue;
            }
            // Source: homeassistant/components/zone/__init__.py  async_active_zone
            let zone_name = find_zone(lat, lon, zones);
            // Source: homeassistant/components/zone/__init__.py  STATE_NOT_HOME = "not_home"
            let state = zone_name.unwrap_or_else(|| "not_home".to_string());
            let mut states = self.states.write().unwrap();
            states.insert(
                person.id.clone(),
                PersonState {
                    person_id: person.id.clone(),
                    state,
                    latitude: Some(lat),
                    longitude: Some(lon),
                    gps_accuracy: Some(accuracy),
                    source: Some(device_id.to_string()),
                },
            );
        }
    }

    /// Get the current state for a specific person.
    pub fn get_state(&self, person_id: &str) -> Option<PersonState> {
        self.states.read().unwrap().get(person_id).cloned()
    }

    /// Return all person states.
    pub fn all_states(&self) -> Vec<PersonState> {
        self.states.read().unwrap().values().cloned().collect()
    }
}

impl Default for PersonStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Zone membership helpers
// ---------------------------------------------------------------------------

/// Return the name of the first zone containing (lat, lon), or None.
///
/// Source: homeassistant/components/zone/__init__.py  async_active_zone
fn find_zone(lat: f64, lon: f64, zones: &[StoredZone]) -> Option<String> {
    for zone in zones {
        let (Some(zlat), Some(zlon)) = (zone.latitude, zone.longitude) else {
            continue;
        };
        let dist = haversine_m(lat, lon, zlat, zlon);
        if dist <= zone.radius {
            return Some(zone.name.clone());
        }
    }
    None
}

/// Haversine great-circle distance in metres.
///
/// Source: homeassistant/helpers/location.py  distance
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().atan2((1.0 - a).sqrt())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zone(name: &str, lat: f64, lon: f64, radius: f64) -> StoredZone {
        StoredZone {
            zone_id: name.to_lowercase(),
            name: name.to_string(),
            latitude: Some(lat),
            longitude: Some(lon),
            radius,
            passive: false,
            icon: None,
        }
    }

    #[test]
    fn haversine_self_is_zero() {
        let d = haversine_m(51.5, -0.1, 51.5, -0.1);
        assert!(d < 0.001, "distance to self should be ~0, got {d}");
    }

    #[test]
    fn haversine_known_distance() {
        // London (51.5074, -0.1278) to Paris (48.8566, 2.3522) ≈ 343 km
        let d = haversine_m(51.5074, -0.1278, 48.8566, 2.3522);
        assert!(
            (d - 343_000.0).abs() < 5_000.0,
            "London-Paris should be ~343 km, got {:.0} m",
            d
        );
    }

    #[test]
    fn find_zone_inside() {
        let zones = vec![make_zone("Work", 37.7749, -122.4194, 200.0)];
        // Slightly inside 200m radius
        let result = find_zone(37.7749, -122.4194, &zones);
        assert_eq!(result, Some("Work".to_string()));
    }

    #[test]
    fn find_zone_outside() {
        let zones = vec![make_zone("Work", 37.7749, -122.4194, 100.0)];
        // Clearly outside (different city)
        let result = find_zone(40.7128, -74.0060, &zones);
        assert_eq!(result, None);
    }

    #[test]
    fn handle_location_update_sets_state() {
        let store = PersonStore::new();
        store.add_or_update(Person {
            id: "alice".to_string(),
            name: "Alice".to_string(),
            device_trackers: vec!["wh001".to_string()],
            user_id: None,
        });
        let zones = vec![make_zone("Home", 37.7749, -122.4194, 500.0)];
        store.handle_location_update("wh001", 37.7749, -122.4194, 5.0, &zones);
        let ps = store.get_state("alice").expect("state should be set");
        assert_eq!(ps.state, "Home");
        assert_eq!(ps.source.as_deref(), Some("wh001"));
    }

    #[test]
    fn handle_location_update_not_home() {
        let store = PersonStore::new();
        store.add_or_update(Person {
            id: "bob".to_string(),
            name: "Bob".to_string(),
            device_trackers: vec!["wh002".to_string()],
            user_id: None,
        });
        let zones = vec![make_zone("Home", 37.7749, -122.4194, 100.0)];
        // Clearly outside all zones
        store.handle_location_update("wh002", 40.7128, -74.0060, 10.0, &zones);
        let ps = store.get_state("bob").expect("state should be set");
        assert_eq!(ps.state, "not_home");
    }

    #[test]
    fn handle_location_update_ignores_unrelated_device() {
        let store = PersonStore::new();
        store.add_or_update(Person {
            id: "carol".to_string(),
            name: "Carol".to_string(),
            device_trackers: vec!["wh003".to_string()],
            user_id: None,
        });
        store.handle_location_update("unrelated_device", 37.7749, -122.4194, 5.0, &[]);
        assert!(store.get_state("carol").is_none());
    }

    #[test]
    fn add_or_update_replaces_existing() {
        let store = PersonStore::new();
        store.add_or_update(Person {
            id: "dave".to_string(),
            name: "Dave Old".to_string(),
            device_trackers: vec![],
            user_id: None,
        });
        store.add_or_update(Person {
            id: "dave".to_string(),
            name: "Dave New".to_string(),
            device_trackers: vec!["wh004".to_string()],
            user_id: None,
        });
        let persons = store.all_persons();
        assert_eq!(persons.len(), 1);
        assert_eq!(persons[0].name, "Dave New");
        assert_eq!(persons[0].device_trackers, vec!["wh004"]);
    }

    #[test]
    fn remove_person() {
        let store = PersonStore::new();
        store.add_or_update(Person {
            id: "eve".to_string(),
            name: "Eve".to_string(),
            device_trackers: vec![],
            user_id: None,
        });
        assert!(store.remove("eve"));
        assert!(store.all_persons().is_empty());
        assert!(!store.remove("eve")); // already gone
    }
}
