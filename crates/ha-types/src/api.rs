//! REST API response types.
//!
//! Source: homeassistant/components/api/__init__.py
//!   - GET /api/          → {"message": "API running."}
//!   - GET /api/config    → Config.as_dict()   (homeassistant/core_config.py)
//!   - GET /api/core/state → {"state": "...", "recorder_state": {...}}

use serde::{Deserialize, Serialize};

/// Response body for `GET /api/`.
///
/// Source: homeassistant/components/api/__init__.py  APIStatusView.get
///   return self.json_message("API running.")
///   → {"message": "API running."}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiStatusResponse {
    pub message: String,
}

impl Default for ApiStatusResponse {
    fn default() -> Self {
        Self {
            message: "API running.".into(),
        }
    }
}

/// Minimal subset of the HA config object returned by `GET /api/config`.
///
/// Source: homeassistant/core_config.py  Config.as_dict()
/// Only the fields that external clients most commonly use are included here;
/// additional fields can be added as compatibility requirements expand.
///
/// Key fields always present in a real HA response:
///   components, config_dir, elevation, language, latitude, longitude,
///   location_name, time_zone, unit_system, version, whitelist_external_dirs,
///   state (CoreState value string)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfigResponse {
    /// HA version string, e.g. "2024.3.0"
    pub version: String,
    /// Human-readable location name.
    pub location_name: String,
    /// IANA time-zone identifier, e.g. "America/New_York"
    pub time_zone: String,
    /// ISO 639-1 language code, e.g. "en"
    pub language: String,
    /// Latitude of the home location.
    pub latitude: f64,
    /// Longitude of the home location.
    pub longitude: f64,
    /// Elevation in metres.
    pub elevation: f64,
    /// Unit system name, e.g. "metric" or "us_customary"
    pub unit_system: UnitSystem,
    /// CoreState value string (same enum as /api/core/state returns).
    pub state: String,
    /// Loaded component names, e.g. ["api", "websocket_api", ...]
    pub components: Vec<String>,
    /// Safe directories accessible to the instance (can be empty).
    #[serde(default)]
    pub whitelist_external_dirs: Vec<String>,
}

/// Unit system descriptor.
///
/// Source: homeassistant/util/unit_system.py  UnitSystem.as_dict()
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitSystem {
    pub length: String,
    pub accumulated_precipitation: String,
    pub mass: String,
    pub pressure: String,
    pub temperature: String,
    pub volume: String,
    pub wind_speed: String,
}

impl UnitSystem {
    /// Metric unit system defaults.
    pub fn metric() -> Self {
        Self {
            length: "km".into(),
            accumulated_precipitation: "mm".into(),
            mass: "g".into(),
            pressure: "Pa".into(),
            temperature: "°C".into(),
            volume: "L".into(),
            wind_speed: "m/s".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden: GET /api/ always returns {"message": "API running."}
    ///
    /// Source: homeassistant/components/api/__init__.py  APIStatusView.get
    ///   `return self.json_message("API running.")`
    ///   which calls HomeAssistantView.json_message → {"message": "..."}
    #[test]
    fn api_status_response_exact_message() {
        let resp = ApiStatusResponse::default();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json["message"], "API running.",
            "GET /api/ message must match HA exactly"
        );
    }

    /// Golden: GET /api/ response must have only a `message` field.
    ///
    /// Source: homeassistant/components/http/__init__.py  HomeAssistantView.json_message
    #[test]
    fn api_status_response_only_message_field() {
        let resp = ApiStatusResponse::default();
        let json = serde_json::to_value(&resp).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(
            obj.len(),
            1,
            "API status response must have exactly one field"
        );
        assert!(obj.contains_key("message"));
    }

    /// Deserialise a real HA /api/ response.
    #[test]
    fn api_status_deserialises() {
        let raw = r#"{"message": "API running."}"#;
        let resp: ApiStatusResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.message, "API running.");
    }

    /// Unit system is always present and has all expected keys.
    ///
    /// Source: homeassistant/util/unit_system.py  UnitSystem.as_dict()
    #[test]
    fn unit_system_fields_present() {
        let us = UnitSystem::metric();
        let json = serde_json::to_value(&us).unwrap();
        for key in &[
            "length",
            "accumulated_precipitation",
            "mass",
            "pressure",
            "temperature",
            "volume",
            "wind_speed",
        ] {
            assert!(
                json.get(key).is_some(),
                "unit_system missing field: {key}"
            );
        }
    }

    /// Round-trip config response.
    #[test]
    fn api_config_round_trip() {
        let cfg = ApiConfigResponse {
            version: "2024.3.0".into(),
            location_name: "Home".into(),
            time_zone: "America/New_York".into(),
            language: "en".into(),
            latitude: 40.7128,
            longitude: -74.0060,
            elevation: 10.0,
            unit_system: UnitSystem::metric(),
            state: "RUNNING".into(),
            components: vec!["api".into(), "websocket_api".into()],
            whitelist_external_dirs: vec![],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: ApiConfigResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.version, cfg.version);
        assert_eq!(decoded.location_name, "Home");
        assert_eq!(decoded.state, "RUNNING");
    }
}
