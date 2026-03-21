//! Entity state type.
//!
//! Source: homeassistant/core.py  `State._as_dict`
//!
//! The REST API renders each state as:
//!   {
//!     "entity_id": "domain.object_id",
//!     "state": "<current value string>",
//!     "attributes": { ... },
//!     "last_changed": "2026-01-01T00:00:00.000000+00:00",
//!     "last_reported": "2026-01-01T00:00:00.000000+00:00",
//!     "last_updated":  "2026-01-01T00:00:00.000000+00:00",
//!     "context": {"id": "...", "parent_id": null, "user_id": null}
//!   }
//!
//! Important constraints from the source:
//! • entity_id must match `^[a-z0-9_]+\.[a-z0-9_]+$`
//!   (homeassistant/core.py  VALID_ENTITY_ID regex)
//! • All three timestamp fields are always present; RFC 3339 / ISO 8601 with
//!   microseconds and timezone offset, e.g. "2026-01-01T12:00:00.000000+00:00"
//! • "context" is always an object (never omitted).
//! • "attributes" is always an object (never null).
//! • The maximum state string length is 255 (MAX_LENGTH_STATE_STATE).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::Context;

/// A single entity state as returned by the HA REST API.
///
/// Source shape: homeassistant/core.py  State._as_dict / as_dict_json
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// "domain.object_id", e.g. "light.living_room"
    pub entity_id: String,
    /// Current state string, max 255 chars, e.g. "on", "off", "unavailable"
    pub state: String,
    /// Arbitrary key→value attributes. Never null; empty object if none.
    pub attributes: HashMap<String, Value>,
    /// ISO 8601 with microseconds and UTC offset.
    pub last_changed: String,
    /// ISO 8601, same format. This field was added later but is always present.
    pub last_reported: String,
    /// ISO 8601, same format.
    pub last_updated: String,
    /// Always present context object.
    pub context: Context,
}

impl State {
    /// Validate the entity_id format.
    ///
    /// Source: homeassistant/core.py  VALID_ENTITY_ID = re.compile(r"^[a-z0-9_]+\.[a-z0-9_]+$")
    ///
    /// Note: the real HA regex allows letters/digits/underscores in both parts,
    /// at least one character each, and exactly one dot separator.
    pub fn is_valid_entity_id(entity_id: &str) -> bool {
        let Some(dot) = entity_id.find('.') else {
            return false;
        };
        let (domain, rest) = entity_id.split_at(dot);
        let object_id = &rest[1..]; // skip the dot
        !domain.is_empty()
            && !object_id.is_empty()
            && domain.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && object_id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------------------------------------------------------------------------
    // entity_id validation – anchored to homeassistant/core.py VALID_ENTITY_ID
    // -------------------------------------------------------------------------

    /// Source: homeassistant/core.py VALID_ENTITY_ID regex
    #[test]
    fn valid_entity_ids() {
        let valid = [
            "light.living_room",
            "switch.kitchen",
            "sensor.temperature_1",
            "binary_sensor.door",
            "a.b",
        ];
        for id in valid {
            assert!(
                State::is_valid_entity_id(id),
                "expected valid: {id}"
            );
        }
    }

    /// Source: homeassistant/core.py VALID_ENTITY_ID regex – invalid cases
    #[test]
    fn invalid_entity_ids() {
        let invalid = [
            "no_dot",
            ".no_domain",
            "no_object_id.",
            "UPPERCASE.nope",
            "light.Has Space",
            "light.Has-Hyphen",
            "",
        ];
        for id in invalid {
            assert!(
                !State::is_valid_entity_id(id),
                "expected invalid: {id}"
            );
        }
    }

    // -------------------------------------------------------------------------
    // State serialisation – anchored to homeassistant/core.py State._as_dict
    // -------------------------------------------------------------------------

    fn sample_state() -> State {
        State {
            entity_id: "light.living_room".into(),
            state: "on".into(),
            attributes: {
                let mut m = HashMap::new();
                m.insert("brightness".into(), json!(255));
                m.insert("friendly_name".into(), json!("Living Room Light"));
                m
            },
            last_changed: "2026-01-01T12:00:00.000000+00:00".into(),
            last_reported: "2026-01-01T12:00:00.000000+00:00".into(),
            last_updated: "2026-01-01T12:00:05.000000+00:00".into(),
            context: Context::new("01JABCDEF01JABCDEF01JABC00"),
        }
    }

    /// Golden: all required HA state fields must be present.
    ///
    /// Source: homeassistant/core.py  State._as_dict
    #[test]
    fn state_serialises_required_fields() {
        let state = sample_state();
        let json = serde_json::to_value(&state).unwrap();

        assert!(json.get("entity_id").is_some());
        assert!(json.get("state").is_some());
        assert!(json.get("attributes").is_some());
        assert!(json.get("last_changed").is_some());
        assert!(json.get("last_reported").is_some());
        assert!(json.get("last_updated").is_some());
        assert!(json.get("context").is_some());

        assert_eq!(json["entity_id"], "light.living_room");
        assert_eq!(json["state"], "on");
        assert_eq!(json["context"]["id"], "01JABCDEF01JABCDEF01JABC00");
        assert!(json["context"]["parent_id"].is_null());
        assert!(json["context"]["user_id"].is_null());
    }

    /// Golden: attributes must be an object, never null.
    ///
    /// Source: homeassistant/core.py  State._as_dict – attributes is always {}
    #[test]
    fn state_attributes_never_null() {
        let state = State {
            entity_id: "sensor.temp".into(),
            state: "21.5".into(),
            attributes: HashMap::new(),
            last_changed: "2026-01-01T00:00:00.000000+00:00".into(),
            last_reported: "2026-01-01T00:00:00.000000+00:00".into(),
            last_updated: "2026-01-01T00:00:00.000000+00:00".into(),
            context: Context::new("x"),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert!(json["attributes"].is_object(), "attributes must be an object");
    }

    /// Round-trip: deserialise what we serialise.
    #[test]
    fn state_round_trip() {
        let original = sample_state();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: State = serde_json::from_str(&json).unwrap();
        assert_eq!(original.entity_id, decoded.entity_id);
        assert_eq!(original.state, decoded.state);
        assert_eq!(original.context, decoded.context);
    }

    /// Deserialise a real HA-shaped JSON blob.
    ///
    /// Fields taken directly from a real HA 2024.x response.
    #[test]
    fn state_deserialises_real_ha_blob() {
        let raw = r#"{
            "entity_id": "sun.sun",
            "state": "above_horizon",
            "attributes": {
                "next_dawn": "2026-01-02T06:00:00.000000+00:00",
                "friendly_name": "Sun"
            },
            "last_changed": "2026-01-01T08:00:00.000000+00:00",
            "last_reported": "2026-01-01T08:00:00.000000+00:00",
            "last_updated": "2026-01-01T08:00:00.000000+00:00",
            "context": {"id": "abcd1234", "parent_id": null, "user_id": null}
        }"#;
        let state: State = serde_json::from_str(raw).unwrap();
        assert_eq!(state.entity_id, "sun.sun");
        assert_eq!(state.state, "above_horizon");
        assert!(state.attributes.contains_key("friendly_name"));
        assert_eq!(state.context.id, "abcd1234");
    }
}
