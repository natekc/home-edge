//! Event type.
//!
//! Source: homeassistant/core.py  `Event._as_dict`
//!
//! The serialised form is:
//!   {
//!     "event_type": "state_changed",
//!     "data":       { ... },
//!     "origin":     "LOCAL",
//!     "time_fired": "2026-01-01T12:00:00.000000+00:00",
//!     "context":    {"id": "...", "parent_id": null, "user_id": null}
//!   }
//!
//! event_type: EventOrigin enum values (homeassistant/core.py EventOrigin):
//!   local   = "LOCAL"
//!   remote  = "REMOTE"

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::Context;

/// Mirror of Python `EventOrigin` enum values.
///
/// Source: homeassistant/core.py  class EventOrigin(enum.Enum)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventOrigin {
    #[serde(rename = "LOCAL")]
    Local,
    #[serde(rename = "REMOTE")]
    Remote,
}

/// A Home Assistant event as serialised in the REST API and WS event stream.
///
/// Source: homeassistant/core.py  Event._as_dict
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event_type: String,
    pub data: HashMap<String, Value>,
    pub origin: EventOrigin,
    /// ISO 8601 with microseconds and UTC offset.
    pub time_fired: String,
    pub context: Context,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Golden: EventOrigin enum values must match Python source exactly.
    ///
    /// Source: homeassistant/core.py  EventOrigin
    ///   local  = "LOCAL"
    ///   remote = "REMOTE"
    #[test]
    fn event_origin_values_match_python_source() {
        assert_eq!(
            serde_json::to_string(&EventOrigin::Local).unwrap(),
            "\"LOCAL\""
        );
        assert_eq!(
            serde_json::to_string(&EventOrigin::Remote).unwrap(),
            "\"REMOTE\""
        );
    }

    /// Golden: Event serialised shape must match homeassistant/core.py Event._as_dict
    #[test]
    fn event_serialises_all_fields() {
        let event = Event {
            event_type: "state_changed".into(),
            data: {
                let mut m = HashMap::new();
                m.insert("entity_id".into(), json!("light.living_room"));
                m
            },
            origin: EventOrigin::Local,
            time_fired: "2026-01-01T12:00:00.000000+00:00".into(),
            context: Context::new("01JABCDEF01JABCDEF01JABC00"),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event_type"], "state_changed");
        assert_eq!(json["origin"], "LOCAL");
        assert!(json.get("data").is_some());
        assert!(json.get("time_fired").is_some());
        assert!(json.get("context").is_some());
    }

    /// Deserialise a real HA-shaped event blob.
    #[test]
    fn event_deserialises_real_ha_blob() {
        let raw = r#"{
            "event_type": "state_changed",
            "data": {"entity_id": "light.living_room"},
            "origin": "LOCAL",
            "time_fired": "2026-01-01T12:00:00.000000+00:00",
            "context": {"id": "abc", "parent_id": null, "user_id": null}
        }"#;
        let event: Event = serde_json::from_str(raw).unwrap();
        assert_eq!(event.event_type, "state_changed");
        assert_eq!(event.origin, EventOrigin::Local);
    }
}
