//! CoreState — the HA server lifecycle state.
//!
//! Source: homeassistant/core.py  `class CoreState(enum.Enum)`
//! The serialised `.value` strings are what `/api/core/state` returns as
//! `state` and what the web frontend and supervisor check.
//!
//! Exact values from Python source:
//!   not_running = "NOT_RUNNING"
//!   starting     = "STARTING"
//!   running      = "RUNNING"
//!   stopping     = "STOPPING"
//!   final_write  = "FINAL_WRITE"
//!   stopped      = "STOPPED"
//!
//! Source: homeassistant/components/api/__init__.py  `APICoreStateView.get`
//! The endpoint returns:
//!   {"state": "<CoreState.value>", "recorder_state": {"migration_in_progress": bool, "migration_is_live": bool}}

use serde::{Deserialize, Serialize};

/// Mirror of Python `CoreState` enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreState {
    #[serde(rename = "NOT_RUNNING")]
    NotRunning,
    #[serde(rename = "STARTING")]
    Starting,
    #[serde(rename = "RUNNING")]
    Running,
    #[serde(rename = "STOPPING")]
    Stopping,
    #[serde(rename = "FINAL_WRITE")]
    FinalWrite,
    #[serde(rename = "STOPPED")]
    Stopped,
}

/// Response body for `GET /api/core/state`.
///
/// Source: homeassistant/components/api/__init__.py  `APICoreStateView`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreStateResponse {
    /// Current lifecycle state, serialised as the CoreState value string.
    pub state: CoreState,
    /// Recorder migration status.
    pub recorder_state: RecorderState,
}

/// Nested recorder state object inside the core-state response.
///
/// Source: homeassistant/components/api/__init__.py  `APICoreStateView`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderState {
    pub migration_in_progress: bool,
    pub migration_is_live: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden: CoreState enum values must serialise to the exact strings HA uses.
    ///
    /// Source: homeassistant/core.py – CoreState enum values.
    /// The web frontend and supervisor compare against these string values.
    #[test]
    fn core_state_values_match_python_source() {
        let cases = [
            (CoreState::NotRunning, "\"NOT_RUNNING\""),
            (CoreState::Starting, "\"STARTING\""),
            (CoreState::Running, "\"RUNNING\""),
            (CoreState::Stopping, "\"STOPPING\""),
            (CoreState::FinalWrite, "\"FINAL_WRITE\""),
            (CoreState::Stopped, "\"STOPPED\""),
        ];
        for (state, expected) in cases {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, expected, "CoreState value mismatch for {state:?}");
        }
    }

    /// Golden: /api/core/state response shape must match HA exactly.
    ///
    /// Source: homeassistant/components/api/__init__.py  APICoreStateView.get
    /// Expected keys: "state", "recorder_state" → {"migration_in_progress", "migration_is_live"}
    #[test]
    fn core_state_response_shape() {
        let resp = CoreStateResponse {
            state: CoreState::Running,
            recorder_state: RecorderState {
                migration_in_progress: false,
                migration_is_live: false,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["state"], "RUNNING");
        assert_eq!(json["recorder_state"]["migration_in_progress"], false);
        assert_eq!(json["recorder_state"]["migration_is_live"], false);
    }

    /// Verify we can round-trip the entire response from JSON.
    #[test]
    fn core_state_response_deserialises() {
        let raw = r#"{"state":"RUNNING","recorder_state":{"migration_in_progress":false,"migration_is_live":false}}"#;
        let resp: CoreStateResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.state, CoreState::Running);
        assert!(!resp.recorder_state.migration_in_progress);
    }
}
