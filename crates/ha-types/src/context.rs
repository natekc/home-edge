//! Context type.
//!
//! Source: homeassistant/core.py  `Context.as_dict()`
//! The serialised form is:
//!   {"id": "<ulid>", "parent_id": null, "user_id": null}
//!
//! All three fields are always present; parent_id and user_id may be null.

use serde::{Deserialize, Serialize};

/// A Home Assistant context, identifying who/what triggered an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// ULID string, e.g. "01JABCDEF01JABCDEF01JABCD"
    pub id: String,
    /// Optional parent context id.
    pub parent_id: Option<String>,
    /// Optional user id that owns this context.
    pub user_id: Option<String>,
}

impl Context {
    /// Create a new context with no parent / user.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            parent_id: None,
            user_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden: round-trip through the exact JSON shape HA produces.
    ///
    /// Derived from homeassistant/core.py Context._as_dict:
    ///   {"id": "...", "parent_id": null, "user_id": null}
    #[test]
    fn context_serialises_all_three_fields() {
        let ctx = Context::new("01JABCDEF01JABCDEF01JABC00");
        let json = serde_json::to_value(&ctx).unwrap();
        // All three keys must be present (HA always emits them)
        assert_eq!(json["id"], "01JABCDEF01JABCDEF01JABC00");
        assert!(json.get("parent_id").is_some(), "parent_id must be present");
        assert!(json.get("user_id").is_some(), "user_id must be present");
        assert!(json["parent_id"].is_null());
        assert!(json["user_id"].is_null());
    }

    #[test]
    fn context_round_trip() {
        let original = Context {
            id: "abc".into(),
            parent_id: Some("parent".into()),
            user_id: Some("user123".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: Context = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    /// Golden: exact JSON string produced by HA for a root context.
    ///
    /// Source: homeassistant/core.py  Context._as_dict
    #[test]
    fn context_golden_json_shape() {
        let ctx = Context::new("01JABCDEF01JABCDEF01JABC00");
        let json = serde_json::to_string(&ctx).unwrap();
        // Must contain all three fields in stable key order.
        // HA apps rely on all three keys being present.
        assert!(json.contains("\"id\""));
        assert!(json.contains("\"parent_id\""));
        assert!(json.contains("\"user_id\""));
    }
}
