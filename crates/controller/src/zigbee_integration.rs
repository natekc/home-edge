//! Zigbee integration: embeds the zigbee2mqtt-rs bridge as a library.
//!
//! # Architecture
//!
//! ```text
//!  ┌──────────────────────┐  ZigbeeEvent (mpsc)   ┌──────────────────┐
//!  │  zigbee2mqtt-rs      │ ───────────────────→   │  ZigbeeIntegration│
//!  │  Bridge::run()       │                        │  event loop       │
//!  │  (tokio::spawn)      │ ←───────────────────   │  (tokio::spawn)   │
//!  └──────────────────────┘  BridgeCommand (mpsc)  └─────────┬────────┘
//!                                                            │
//!                                         ZigbeeDeviceStore ┤ ZigbeeEntityStore
//!                                              StateStore   ─┘
//! ```
//!
//! The caller (AppState::new_initialized) calls [`ZigbeeIntegration::start`],
//! which spawns both tasks and returns a [`ZigbeeHandle`] for operational
//! control (permit-join, stop).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use zigbee2mqtt_rs::bridge::Bridge;
use zigbee2mqtt_rs::config::{AdapterType, AdvancedConfig, Config, MqttConfig, SerialConfig};
use zigbee2mqtt_rs::events::{BridgeCommand, ZigbeeEvent};

use crate::config::ZigbeeConfig;
use crate::state_store::{now_iso8601, StateStore};
use crate::zigbee_device_store::{ZigbeeDeviceRecord, ZigbeeDeviceStore};
use crate::zigbee_entity_store::{ZigbeeEntityRecord, ZigbeeEntityStore};

// ---------------------------------------------------------------------------
// ZigbeeHandle — operational handle returned to AppState
// ---------------------------------------------------------------------------

/// Handle allowing the HTTP layer to control the running bridge.
///
/// Cloning is cheap — all shared state is behind `Arc`.
#[derive(Clone)]
pub struct ZigbeeHandle {
    cmd_tx: mpsc::Sender<BridgeCommand>,
    /// Unix-epoch seconds when permit-join expires.  0 = not active.
    /// Written from the HTTP layer; read by the status endpoint.
    permit_join_expires_at: Arc<AtomicU64>,
    /// Set by the bridge task if it exits with an error (e.g. serial port not found).
    pub bridge_error: Arc<Mutex<Option<String>>>,
    /// The serial port path from config, for display in diagnostics.
    pub serial_port: String,
}

impl ZigbeeHandle {
    /// Enable device pairing for `duration` seconds (0 = disable immediately).
    ///
    /// Zigbee permit-join works at the coordinator level and is independent of
    /// the HTTP transport — it works equally under WiFi and BLE transports.
    pub async fn permit_join(&self, duration: u8) -> anyhow::Result<()> {
        // Track expiry locally so the status endpoint can serve a countdown
        // without needing to round-trip through the bridge.
        let expires = if duration > 0 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_add(duration as u64)
        } else {
            0
        };
        self.permit_join_expires_at.store(expires, Ordering::Relaxed);
        self.cmd_tx
            .send(BridgeCommand::PermitJoin { duration })
            .await
            .map_err(|_| anyhow::anyhow!("bridge task has stopped"))
    }

    /// Returns the number of seconds until permit-join expires, or 0 if inactive.
    pub fn permit_join_remaining_secs(&self) -> u8 {
        let expires = self.permit_join_expires_at.load(Ordering::Relaxed);
        if expires == 0 {
            return 0;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now >= expires {
            return 0;
        }
        (expires - now).min(u8::MAX as u64) as u8
    }

    /// Shut down the bridge gracefully.
    pub async fn stop(&self) -> anyhow::Result<()> {
        self.permit_join_expires_at.store(0, Ordering::Relaxed);
        self.cmd_tx
            .send(BridgeCommand::Stop)
            .await
            .map_err(|_| anyhow::anyhow!("bridge task has stopped"))
    }

    /// Returns the bridge error message if the bridge task has crashed, otherwise `None`.
    pub fn bridge_error(&self) -> Option<String> {
        self.bridge_error.lock().ok()?.clone()
    }
}

// ---------------------------------------------------------------------------
// Cluster → entity mapping (mirrors zigbee2mqtt-rs/src/homeassistant.rs)
// ---------------------------------------------------------------------------

/// Map IAS Zone Type (ZCL attribute 0x0001 in cluster 0x0500) to an HA binary_sensor device_class.
///
/// Source: homeassistant/components/zha/sensor.py IAS_ZONE_TYPE_MAP
/// Source: homeassistant/components/binary_sensor/__init__.py BinarySensorDeviceClass
fn ias_device_class(zone_type: Option<u16>) -> &'static str {
    match zone_type {
        Some(0x000d) => "motion",          // Motion sensor
        Some(0x0015) => "door",            // Contact switch (door/window)
        Some(0x0028) => "smoke",           // Fire sensor
        Some(0x002a) => "moisture",        // Water sensor
        Some(0x002b) => "carbon_monoxide", // CO sensor
        Some(0x002c) => "safety",          // Personal emergency
        Some(0x002d) => "vibration",       // Vibration/movement sensor
        _            => "opening",         // Fallback for unknown / not-yet-reported types
    }
}

/// Map a binary_sensor device_class to a Material Design icon name.
///
/// Source: homeassistant/components/binary_sensor/__init__.py icon mapping
fn binary_sensor_icon(device_class: &str) -> &'static str {
    match device_class {
        "motion"           => "motion-sensor",
        "door" | "opening" | "window" => "door-closed",
        "smoke"            => "smoke-detector",
        "moisture"         => "water",
        "carbon_monoxide"  => "molecule-co",
        "vibration"        => "vibrate",
        "occupancy"        => "motion-sensor",
        "tamper"           => "shield-alert",
        "battery"          => "battery-alert",
        "safety"           => "shield-account",
        _                  => "alert-circle-outline",
    }
}

/// Derive HA entity records from a device's ZCL input clusters.
///
/// Mirrors the discovery logic in `zigbee2mqtt-rs/src/homeassistant.rs` so
/// that entity domains/classes are consistent with what zigbee2mqtt would
/// expose via MQTT, but without requiring a broker.
pub fn entities_for_device(device: &zigbee2mqtt_rs::DeviceInfo) -> Vec<ZigbeeEntityRecord> {
    let clusters = device.all_input_clusters();
    let mut records: Vec<ZigbeeEntityRecord> = Vec::new();

    let ieee = device.ieee_addr.as_hex();
    let base = device
        .friendly_name
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric() && c != '_', "_");

    let has_on_off    = clusters.contains(&0x0006);
    let has_level     = clusters.contains(&0x0008);
    let has_color     = clusters.contains(&0x0300);
    let has_temperature  = clusters.contains(&0x0402);
    let has_humidity     = clusters.contains(&0x0405);
    let has_occupancy    = clusters.contains(&0x0406);
    let has_power        = clusters.contains(&0x0001);
    let has_illuminance  = clusters.contains(&0x0400);
    let has_pressure     = clusters.contains(&0x0403);
    let has_ias          = clusters.contains(&0x0500);
    // Source: homeassistant/components/zha/sensor.py SmartEnergySummation
    let has_metering     = clusters.contains(&0x0702);
    // Source: homeassistant/components/zha/sensor.py ElectricalMeasurement
    let has_elec_meas    = clusters.contains(&0x0B04);

    // ── Light or Switch ──────────────────────────────────────────────────
    if has_on_off {
        let domain = if has_level || has_color { "light" } else { "switch" };
        records.push(ZigbeeEntityRecord {
            entity_id: format!("{domain}.{base}"),
            ieee_addr: ieee.clone(),
            domain: domain.to_string(),
            attribute_key: None, // uses "state" key directly
            device_class: None,
            unit_of_measurement: None,
            name_by_user: None,
            user_area_id: None,
        });
    }

    // ── Sensors ──────────────────────────────────────────────────────────
    if has_temperature {
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_temperature"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            attribute_key: Some("temperature".to_string()),
            device_class: Some("temperature".to_string()),
            unit_of_measurement: Some("°C".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
    }

    if has_humidity {
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_humidity"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            attribute_key: Some("humidity".to_string()),
            device_class: Some("humidity".to_string()),
            unit_of_measurement: Some("%".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
    }

    if has_pressure {
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_pressure"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            // Cluster 0x0403: MeasuredValue reported as "pressure" in hPa
            attribute_key: Some("pressure".to_string()),
            device_class: Some("atmospheric_pressure".to_string()),
            unit_of_measurement: Some("hPa".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
    }

    if has_power {
        // Battery percentage — primary sensor.
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_battery"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            attribute_key: Some("battery".to_string()),
            device_class: Some("battery".to_string()),
            unit_of_measurement: Some("%".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
        // Battery voltage — secondary sensor.
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_battery_voltage"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            attribute_key: Some("battery_voltage".to_string()),
            device_class: Some("voltage".to_string()),
            unit_of_measurement: Some("V".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
        // Low-battery warning — binary sensor.
        records.push(ZigbeeEntityRecord {
            entity_id: format!("binary_sensor.{base}_battery_low"),
            ieee_addr: ieee.clone(),
            domain: "binary_sensor".to_string(),
            attribute_key: Some("battery_low".to_string()),
            device_class: Some("battery".to_string()),
            unit_of_measurement: None,
            name_by_user: None,
            user_area_id: None,
        });
    }

    if has_illuminance {
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_illuminance"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            // Cluster 0x0400 emits "illuminance_lux" (converted from log scale)
            attribute_key: Some("illuminance_lux".to_string()),
            device_class: Some("illuminance".to_string()),
            unit_of_measurement: Some("lx".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
    }

    // ── Binary sensors ────────────────────────────────────────────────────
    if has_occupancy {
        records.push(ZigbeeEntityRecord {
            entity_id: format!("binary_sensor.{base}_occupancy"),
            ieee_addr: ieee.clone(),
            domain: "binary_sensor".to_string(),
            attribute_key: Some("occupancy".to_string()),
            device_class: Some("occupancy".to_string()),
            unit_of_measurement: None,
            name_by_user: None,
            user_area_id: None,
        });
    }

    if has_ias {
        // IAS Zone (0x0500): zone_type attribute (0x0001) identifies the sensor kind.
        // Read from initial_state if available; fall back to "opening".
        // Source: homeassistant/components/zha/sensor.py IAS_ZONE_TYPE_MAP
        let zone_type: Option<u16> = device
            .initial_state
            .get("zone_type")
            .and_then(|v| v.as_u64())
            .map(|v| v as u16);
        let dc = ias_device_class(zone_type);

        records.push(ZigbeeEntityRecord {
            entity_id: format!("binary_sensor.{base}_contact"),
            ieee_addr: ieee.clone(),
            domain: "binary_sensor".to_string(),
            attribute_key: Some("contact".to_string()),
            device_class: Some(dc.to_string()),
            unit_of_measurement: None,
            name_by_user: None,
            user_area_id: None,
        });
        records.push(ZigbeeEntityRecord {
            entity_id: format!("binary_sensor.{base}_tamper"),
            ieee_addr: ieee.clone(),
            domain: "binary_sensor".to_string(),
            attribute_key: Some("tamper".to_string()),
            device_class: Some("tamper".to_string()),
            unit_of_measurement: None,
            name_by_user: None,
            user_area_id: None,
        });
    }

    // ── Energy metering (ZCL 0x0702 Smart Energy Metering) ───────────────
    if has_metering {
        // Cumulative energy consumption in kWh.
        // Source: homeassistant/components/zha/sensor.py SmartEnergySummation
        // Source: homeassistant/const.py UnitOfEnergy.KILO_WATT_HOUR = "kWh"
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_energy"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            attribute_key: Some("energy".to_string()),
            device_class: Some("energy".to_string()),
            unit_of_measurement: Some("kWh".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
    }

    // ── Electrical measurement (ZCL 0x0B04 Electrical Measurement) ────────
    if has_elec_meas {
        // Instantaneous active power in watts.
        // Source: homeassistant/components/zha/sensor.py ElectricalMeasurementActivePower
        // Source: homeassistant/const.py UnitOfPower.WATT = "W"
        // Source: homeassistant/components/sensor/__init__.py SensorDeviceClass.POWER
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_power"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            attribute_key: Some("power".to_string()),
            device_class: Some("power".to_string()),
            unit_of_measurement: Some("W".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
        // RMS voltage in volts.
        // Source: homeassistant/components/zha/sensor.py ElectricalMeasurementRMSVoltage
        // Source: homeassistant/const.py UnitOfElectricPotential.VOLT = "V"
        // Source: homeassistant/components/sensor/__init__.py SensorDeviceClass.VOLTAGE
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_voltage"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            attribute_key: Some("voltage".to_string()),
            device_class: Some("voltage".to_string()),
            unit_of_measurement: Some("V".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
        // RMS current in amperes.
        // Source: homeassistant/components/zha/sensor.py ElectricalMeasurementRMSCurrent
        // Source: homeassistant/const.py UnitOfElectricCurrent.AMPERE = "A"
        // Source: homeassistant/components/sensor/__init__.py SensorDeviceClass.CURRENT
        records.push(ZigbeeEntityRecord {
            entity_id: format!("sensor.{base}_current"),
            ieee_addr: ieee.clone(),
            domain: "sensor".to_string(),
            attribute_key: Some("current".to_string()),
            device_class: Some("current".to_string()),
            unit_of_measurement: Some("A".to_string()),
            name_by_user: None,
            user_area_id: None,
        });
    }

    records
}

// ---------------------------------------------------------------------------
// State push helper
// ---------------------------------------------------------------------------

use ha_types::entity::State;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

fn new_ctx() -> ha_types::context::Context {
    ha_types::context::Context::new(Uuid::new_v4().to_string())
}

/// Convert HS (hue 0–360, saturation 0–100) to sRGB at maximum brightness.
/// Source: homeassistant/util/color.py color_hs_to_RGB
fn hs_to_rgb(h: f32, s: f32) -> (u8, u8, u8) {
    let s = s / 100.0;
    let h = h / 60.0;
    let i = h.floor() as u32;
    let f = h - i as f32;
    let p = 1.0 - s;
    let q = 1.0 - s * f;
    let t = 1.0 - s * (1.0 - f);
    let (r, g, b) = match i % 6 {
        0 => (1.0_f32, t, p),
        1 => (q, 1.0_f32, p),
        2 => (p, 1.0_f32, t),
        3 => (p, q, 1.0_f32),
        4 => (t, p, 1.0_f32),
        _ => (1.0_f32, p, q),
    };
    ((r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8)
}

/// Convert a raw ZCL state map into `ha_types::State` entries and push them
/// into the `StateStore`.
///
/// - `light` / `switch`: the `"state"` key maps to entity state ("on"/"off");
///   brightness, color_temp, etc. become attributes.
/// - `sensor` / `binary_sensor`: `attribute_key` is looked up in `raw_state`
///   and becomes the entity state value.
pub fn push_state(
    raw_state: &serde_json::Map<String, serde_json::Value>,
    entities: &[ZigbeeEntityRecord],
    state_store: &StateStore,
) {
    for ent in entities {
        let state_value: Option<String> = match ent.domain.as_str() {
            "light" | "switch" => raw_state
                .get("state")
                .and_then(|v| v.as_str())
                .map(|s| s.to_lowercase()),
            _ => ent
                .attribute_key
                .as_deref()
                .and_then(|k| raw_state.get(k))
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => if *b { "on".to_string() } else { "off".to_string() },
                    other => other.to_string(),
                }),
        };

        let Some(sv) = state_value else { continue };

        // Build attributes map.
        let mut attrs: HashMap<String, Value> = HashMap::new();
        if let Some(ref unit) = ent.unit_of_measurement {
            attrs.insert("unit_of_measurement".into(), Value::String(unit.clone()));
        }
        if let Some(ref dc) = ent.device_class {
            attrs.insert("device_class".into(), Value::String(dc.clone()));
        }

        match ent.domain.as_str() {
            "sensor" => {
                // Source: homeassistant/components/sensor/__init__.py SensorStateClass
                // energy (cumulative kWh from metering cluster) uses total_increasing;
                // all other numeric sensors use measurement.
                // Source: homeassistant/components/zha/sensor.py SmartEnergySummation
                let state_class = if ent.device_class.as_deref() == Some("energy") {
                    "total_increasing"
                } else {
                    "measurement"
                };
                attrs.insert("state_class".into(), Value::String(state_class.into()));
            }
            "light" => {
                // Brightness (0-254 raw from ZCL level cluster).
                for key in ["brightness", "color_mode", "color"] {
                    if let Some(v) = raw_state.get(key) {
                        attrs.insert(key.to_string(), v.clone());
                    }
                }
                // Convert color_temp from mireds to kelvin (1_000_000 / mireds).
                // ZCL/z2m reports color_temp in mireds; HA frontend expects
                // color_temp_kelvin.
                // Source: homeassistant/components/light/__init__.py
                if let Some(mireds) = raw_state.get("color_temp").and_then(|v| v.as_f64()) {
                    if mireds > 0.0 {
                        let kelvin = (1_000_000.0 / mireds).round() as u32;
                        attrs.insert("color_temp_kelvin".into(), Value::Number(kelvin.into()));
                        attrs.insert("color_temp".into(), Value::Number(
                            serde_json::Number::from_f64(mireds).unwrap_or(serde_json::Number::from(0)),
                        ));
                    }
                }
                // Extract hue/saturation from the nested color object and emit as
                // HA-standard hs_color [h, s] array + rgb_color [r, g, b] array.
                // Source: homeassistant/components/light/__init__.py ATTR_HS_COLOR, ATTR_RGB_COLOR
                if let Some(Value::Object(color_obj)) = raw_state.get("color") {
                    let h = color_obj.get("hue").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    let s = color_obj.get("saturation").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    attrs.insert("hs_color".into(), serde_json::json!([h, s]));
                    // Source: homeassistant/util/color.py color_hs_to_RGB
                    let (r, g, b) = hs_to_rgb(h, s);
                    attrs.insert("rgb_color".into(), serde_json::json!([r, g, b]));
                }
            }
            _ => {}
        }

        // Source: homeassistant/helpers/entity_registry.py RegistryEntry.name
        // The HA Companion app reads display name from attributes["friendly_name"].
        attrs.insert("friendly_name".into(), Value::String(ent.display_name()));

        let ts = now_iso8601();
        let state = State {
            entity_id: ent.entity_id.clone(),
            state: sv,
            attributes: attrs,
            last_changed: ts.clone(),
            last_updated: ts.clone(),
            last_reported: ts,
            context: new_ctx(),
        };

        if let Err(e) = state_store.set(state) {
            warn!("failed to set state for {}: {e}", ent.entity_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Integration runner
// ---------------------------------------------------------------------------

/// Record numeric sensor readings to the history store for all sensor-domain
/// entities whose attribute key is present and numeric in `raw_state`.
async fn record_sensor_history(
    raw_state: &serde_json::Map<String, serde_json::Value>,
    entities: &[crate::zigbee_entity_store::ZigbeeEntityRecord],
    history: &crate::history_store::HistoryStore,
) {
    for ent in entities {
        match ent.domain.as_str() {
            "sensor" => {
                let Some(key) = ent.attribute_key.as_deref() else { continue };
                let Some(raw) = raw_state.get(key) else { continue };
                let Some(v) = raw.as_f64().or_else(|| raw.as_str().and_then(|s| s.parse().ok())) else {
                    continue
                };
                history.record(&ent.entity_id, v).await;
            }
            // Source: homeassistant/components/history/__init__.py binary sensor tracking
            "binary_sensor" => {
                let Some(key) = ent.attribute_key.as_deref() else { continue };
                let Some(raw) = raw_state.get(key) else { continue };
                let v = match raw {
                    Value::String(s) => match s.to_lowercase().as_str() {
                        "on" | "true"  => 1.0,
                        "off" | "false" => 0.0,
                        _ => continue,
                    },
                    Value::Bool(b) => if *b { 1.0 } else { 0.0 },
                    _ => continue,
                };
                history.record(&ent.entity_id, v).await;
            }
            // Source: homeassistant/components/history/__init__.py binary sensor tracking
            "light" | "switch" => {
                let Some(raw) = raw_state.get("state") else { continue };
                let v = match raw {
                    Value::String(s) => match s.to_lowercase().as_str() {
                        "on"  => 1.0,
                        "off" => 0.0,
                        _ => continue,
                    },
                    _ => continue,
                };
                history.record(&ent.entity_id, v).await;
            }
            _ => {}
        }
    }
}

/// Restore last-known sensor states from the history database into the
/// in-memory `StateStore`.
///
/// Called once at startup (after the bridge is launched) so that entities
/// show their last measured value immediately — rather than "unavailable" —
/// until the physical sensor next transmits.
///
/// Mirrors Home Assistant's `RestoreEntity` / `restore_state` mechanism
/// (homeassistant/helpers/restore_state.py).
pub async fn restore_states_from_history(
    entity_store: &ZigbeeEntityStore,
    history: &crate::history_store::HistoryStore,
    state_store: &StateStore,
) {
    let entities = match entity_store.list().await {
        Ok(e) => e,
        Err(e) => {
            warn!("restore_states: could not load entities: {e:#}");
            return;
        }
    };
    // Build lookup: entity_id → last value from history.
    let latest = history.latest_values().await;
    let value_map: std::collections::HashMap<&str, f64> =
        latest.iter().map(|(id, v)| (id.as_str(), *v)).collect();

    let ts = now_iso8601();
    let mut restored = 0usize;
    for ent in &entities {
        match ent.domain.as_str() {
            "sensor" => {
                let Some(&v) = value_map.get(ent.entity_id.as_str()) else { continue };

                let sv = if v == v.trunc() && (v.abs() < 1e9) {
                    format!("{v:.0}")
                } else {
                    format!("{v:.2}").trim_end_matches('0').trim_end_matches('.').to_string()
                };

                let mut attrs: std::collections::HashMap<String, serde_json::Value> =
                    std::collections::HashMap::new();
                if let Some(ref unit) = ent.unit_of_measurement {
                    attrs.insert("unit_of_measurement".into(), serde_json::Value::String(unit.clone()));
                }
                if let Some(ref dc) = ent.device_class {
                    attrs.insert("device_class".into(), serde_json::Value::String(dc.clone()));
                }
                // Source: homeassistant/components/sensor/__init__.py SensorStateClass
                // energy device class uses total_increasing (cumulative metering counter).
                let sc = if ent.device_class.as_deref() == Some("energy") {
                    "total_increasing"
                } else {
                    "measurement"
                };
                attrs.insert("state_class".into(), serde_json::Value::String(sc.into()));
                attrs.insert("restored".into(), serde_json::Value::Bool(true));
                // Source: homeassistant/helpers/entity_registry.py RegistryEntry.name
                attrs.insert("friendly_name".into(), serde_json::Value::String(ent.display_name()));

                let state = ha_types::entity::State {
                    entity_id: ent.entity_id.clone(),
                    state: sv,
                    attributes: attrs,
                    last_changed: ts.clone(),
                    last_updated: ts.clone(),
                    last_reported: ts.clone(),
                    context: new_ctx(),
                };
                if let Err(e) = state_store.set(state) {
                    warn!("restore_states: failed to set {}: {e}", ent.entity_id);
                } else {
                    restored += 1;
                }
            }
            // Source: homeassistant/helpers/restore_state.py RestoreEntity.async_get_last_state
            // light/switch states are stored as 1.0/0.0 in history; restore as "on"/"off".
            "light" | "switch" => {
                let Some(&v) = value_map.get(ent.entity_id.as_str()) else { continue };
                let sv = if v >= 0.5 { "on" } else { "off" };

                let mut attrs: std::collections::HashMap<String, serde_json::Value> =
                    std::collections::HashMap::new();
                attrs.insert("restored".into(), serde_json::Value::Bool(true));
                // Source: homeassistant/helpers/entity_registry.py RegistryEntry.name
                attrs.insert("friendly_name".into(), serde_json::Value::String(ent.display_name()));

                let state = ha_types::entity::State {
                    entity_id: ent.entity_id.clone(),
                    state: sv.to_string(),
                    attributes: attrs,
                    last_changed: ts.clone(),
                    last_updated: ts.clone(),
                    last_reported: ts.clone(),
                    context: new_ctx(),
                };
                if let Err(e) = state_store.set(state) {
                    warn!("restore_states: failed to set {}: {e}", ent.entity_id);
                } else {
                    restored += 1;
                }
            }
            _ => {}
        }
    }
    let total = entities.iter().filter(|e| matches!(e.domain.as_str(), "sensor" | "light" | "switch")).count();
    info!("restore_states: restored {restored}/{total} states from history");
}

/// Consume events from `event_rx` and fan them out to the stores.
///
/// This is the inner loop extracted so it can be driven from tests without
/// spinning up a real `Bridge`. Call it inside a `tokio::spawn` block.
pub async fn run_event_loop(
    mut event_rx: mpsc::Receiver<ZigbeeEvent>,
    device_store: Arc<ZigbeeDeviceStore>,
    entity_store: Arc<ZigbeeEntityStore>,
    state_store: Arc<StateStore>,
    history_store: Arc<crate::history_store::HistoryStore>,
    logbook_store: Arc<crate::logbook_store::LogbookStore>,
) {
    while let Some(event) = event_rx.recv().await {
        match event {
            ZigbeeEvent::DeviceJoined { ieee_addr, .. } => {
                let ieee = ieee_addr.as_hex();
                info!("Zigbee device joined: {ieee}");
                // Create a minimal record (interview not yet complete).
                let record = ZigbeeDeviceRecord {
                    ieee_addr: ieee,
                    friendly_name: ieee_addr.as_hex(),
                    manufacturer: None,
                    model: None,
                    power_source: None,
                    sw_build_id: None,
                    interview_complete: false,
                    last_seen: None,
                    name_by_user: None,
                    user_area_id: None,
                };
                if let Err(e) = device_store.upsert(record).await {
                    warn!("zigbee device store upsert failed: {e:#}");
                }
            }

            ZigbeeEvent::DeviceLeft { ieee_addr } => {
                let ieee = ieee_addr.as_hex();
                info!("Zigbee device left: {ieee}");
                // Mark unavailable in state store (do NOT delete — mirrors HA behaviour).
                if let Ok(entities) = entity_store.list_for_device(&ieee).await {
                    for ent in entities {
                        let _ = state_store.set(ha_types::entity::State {
                            entity_id: ent.entity_id.clone(),
                            state: "unavailable".to_string(),
                            attributes: Default::default(),
                            last_changed: String::new(),
                            last_updated: String::new(),
                            last_reported: String::new(),
                            context: new_ctx(),
                        });
                    }
                }
            }

            ZigbeeEvent::DeviceInterviewComplete { info } => {
                let ieee = info.ieee_addr.as_hex();
                info!("Zigbee interview complete: {ieee} model={:?}", info.model);

                // Upsert full device record.
                let record = ZigbeeDeviceRecord {
                    ieee_addr: ieee.clone(),
                    friendly_name: info.friendly_name.clone(),
                    manufacturer: info.manufacturer.clone(),
                    model: info.model.clone(),
                    power_source: info.power_source.clone(),
                    sw_build_id: info.sw_build_id.clone(),
                    interview_complete: true,
                    last_seen: None,
                    name_by_user: None,
                    user_area_id: None,
                };
                if let Err(e) = device_store.upsert(record).await {
                    warn!("zigbee device store upsert failed: {e:#}");
                }

                // Derive and register entities.
                let entity_records = entities_for_device(&info);
                if let Err(e) = entity_store.register_bulk(entity_records.clone()).await {
                    warn!("zigbee entity store register failed: {e:#}");
                }

                // Push initial state from the device's cached values.
                push_state(&info.initial_state, &entity_records, &state_store);
                record_sensor_history(&info.initial_state, &entity_records, &history_store).await;
            }

            ZigbeeEvent::StateChanged { ieee_addr, delta } => {
                let ieee = ieee_addr.as_hex();
                if let Ok(entities) = entity_store.list_for_device(&ieee).await {
                    // Source: homeassistant/components/logbook/__init__.py Event.LOGBOOK_ENTRY
                    // Capture old state values before push so we can record logbook entries
                    // only for entities whose state value actually changed.
                    let old_values: Vec<(String, String)> = entities
                        .iter()
                        .filter_map(|ent| {
                            state_store
                                .get(&ent.entity_id)
                                .map(|s| (ent.entity_id.clone(), s.state.clone()))
                        })
                        .collect();

                    push_state(&delta, &entities, &state_store);
                    record_sensor_history(&delta, &entities, &history_store).await;

                    // Write logbook entries for entities whose state value changed.
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    for (entity_id, old_sv) in &old_values {
                        if let Some(new_state) = state_store.get(entity_id) {
                            if new_state.state != *old_sv {
                                let display_name = new_state
                                    .attributes
                                    .get("friendly_name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(entity_id)
                                    .to_string();
                                logbook_store.record(crate::logbook_store::LogbookEntry {
                                    ts,
                                    entity_id: entity_id.clone(),
                                    display_name,
                                    old_state: old_sv.clone(),
                                    new_state: new_state.state.clone(),
                                }).await;
                            }
                        }
                    }
                }
                // Record freshness — mirrors HA's last_seen tracking on state updates.
                let ts = now_iso8601();
                let _ = device_store.touch_last_seen(&ieee, ts).await;
            }

            // Forward-compatibility: ignore any event variants added in future
            // zigbee2mqtt-rs releases (ZigbeeEvent is #[non_exhaustive]).
            _ => {}
        }
    }
    info!("Zigbee event loop ended");
}

/// Spawn the bridge + event-fan-out tasks and return a `ZigbeeHandle`.
///
/// Zigbee is transport-agnostic: it communicates with the coordinator over a
/// local serial connection.  The HTTP transport (WiFi vs BLE) is irrelevant
/// to whether pairing, state updates, or device interviews work.
pub async fn start(
    cfg: ZigbeeConfig,
    device_store: Arc<ZigbeeDeviceStore>,
    entity_store: Arc<ZigbeeEntityStore>,
    state_store: Arc<StateStore>,
    history_store: Arc<crate::history_store::HistoryStore>,
    logbook_store: Arc<crate::logbook_store::LogbookStore>,
) -> ZigbeeHandle {
    // Build the zigbee2mqtt-rs Config from our ZigbeeConfig.
    let z2m_cfg = Config {
        serial: SerialConfig {
            port: cfg.serial_port.clone(),
            baudrate: cfg.baudrate,
            adapter: match cfg.adapter.to_lowercase().as_str() {
                "znp" => AdapterType::Znp,
                "ezsp" => AdapterType::Ezsp,
                _ => AdapterType::Auto,
            },
            rtscts: cfg.rtscts,
        },
        mqtt: MqttConfig { enabled: false, ..Default::default() }, // no broker — events come via the notify channel
        permit_join: cfg.permit_join_on_startup,
        homeassistant: false, // home-edge handles HA entity exposure
        devices: Default::default(),
        advanced: AdvancedConfig {
            channel: cfg.channel,
            pan_id: cfg.pan_id.unwrap_or(0x1a62),
            ..Default::default()
        },
    };

    // Create bridge with event + command channels.
    let config_path = std::path::PathBuf::from("/dev/null"); // not used without MQTT/DB
    let (bridge, event_rx, cmd_tx) = Bridge::new_with_channels(z2m_cfg, config_path);

    // Shared slot for bridge task errors (e.g. serial port not found).
    let bridge_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Spawn the bridge task.  Capture any startup error so the HTTP layer
    // can surface it in the diagnostics UI (e.g. serial port not found).
    let bridge_error_tx = Arc::clone(&bridge_error);
    tokio::spawn(async move {
        if let Err(e) = bridge.run().await {
            let msg = format!("{e:#}");
            error!("Zigbee bridge exited with error: {msg}");
            if let Ok(mut guard) = bridge_error_tx.lock() {
                *guard = Some(msg);
            }
        }
    });

    // Spawn the event fan-out task.  After the loop exits (coordinator
    // disconnected), mark all known Zigbee entities unavailable.
    // Source: homeassistant/components/zha/core/gateway.py ZHAGateway.async_disconnect
    tokio::spawn(async move {
        run_event_loop(
            event_rx,
            Arc::clone(&device_store),
            Arc::clone(&entity_store),
            Arc::clone(&state_store),
            Arc::clone(&history_store),
            Arc::clone(&logbook_store),
        ).await;
        // When the coordinator disconnects, mark all Zigbee entities unavailable.
        if let Ok(entities) = entity_store.list().await {
            let count = entities.len();
            let ts = now_iso8601();
            for ent in entities {
                if let Some(mut st) = state_store.get(&ent.entity_id) {
                    st.state = "unavailable".to_string();
                    st.last_updated = ts.clone();
                    let _ = state_store.set(st);
                }
            }
            info!("Zigbee coordinator disconnected: marked {count} entities unavailable");
        }
    });

    ZigbeeHandle {
        cmd_tx,
        permit_join_expires_at: Arc::new(AtomicU64::new(0)),
        bridge_error,
        serial_port: cfg.serial_port.clone(),
    }
}

// ---------------------------------------------------------------------------
// Presentation helpers (HTTP / transport_wifi only)
// ---------------------------------------------------------------------------
//
// Zigbee itself (pairing, state collection, state push) is transport-neutral —
// it works whether the client connects over WiFi or BLE.  The functions below
// are only needed by the HTTP rendering layer, so they are compiled only when
// the `transport_wifi` feature is active.  On a headless BLE-only build the
// Zigbee integration still works; only the HTML templates are absent.

/// Build an [`EntityView`][crate::entity_view::EntityView] from a Zigbee entity
/// record and the live state store.
///
/// Zigbee-specific presentation logic (icon selection, service-action rules)
/// lives here rather than in `http.rs` so the HTTP layer stays decoupled from
/// Zigbee internals.  The HTTP compositor calls this via
/// `crate::http::fetch_entity_view`.
///
/// # Pattern for future backends
/// A WiFi-sensor backend would add an analogous function in its own module:
/// ```ignore
/// // wifi_sensor_store.rs
/// pub(crate) fn entity_view_for(
///     entity: &WifiSensorRecord,
///     states: &crate::state_store::StateStore,
/// ) -> crate::entity_view::EntityView { ... }
/// ```
/// Then `fetch_entity_view` gets one new `#[cfg(feature = "wifi_sensors")]` arm.
///
/// Compiled only when `transport_wifi` is active (requires the HTTP layer and
/// `crate::entity_view`).  Zigbee data collection works on all transports.
#[cfg(feature = "transport_wifi")]
pub(crate) fn entity_view_for(
    entity: &ZigbeeEntityRecord,
    states: &StateStore,
    device_name: Option<String>,
) -> crate::entity_view::EntityView {
    let value = states
        .get(&entity.entity_id)
        .map(|s| s.state.clone())
        .unwrap_or_else(|| "unavailable".to_string());
    let attrs = states
        .get(&entity.entity_id)
        .map(|s| s.attributes)
        .unwrap_or_default();

    let brightness = attrs
        .get("brightness")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(255) as u8);
    let color_temp_kelvin = attrs
        .get("color_temp_kelvin")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16);
    let min_color_temp_kelvin = attrs
        .get("min_color_temp_kelvin")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .unwrap_or(2000);
    let max_color_temp_kelvin = attrs
        .get("max_color_temp_kelvin")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .unwrap_or(6535);
    // Source: homeassistant/components/light/__init__.py ATTR_COLOR_MODE
    let color_mode = attrs
        .get("color_mode")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // Source: homeassistant/components/light/__init__.py ATTR_HS_COLOR
    let hs_color = attrs
        .get("hs_color")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            if arr.len() == 2 {
                let h = arr[0].as_f64()? as f32;
                let s = arr[1].as_f64()? as f32;
                Some((h, s))
            } else { None }
        });
    // Source: homeassistant/components/light/__init__.py ATTR_RGB_COLOR
    let rgb_color = attrs
        .get("rgb_color")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            if arr.len() == 3 {
                let r = arr[0].as_u64()? as u8;
                let g = arr[1].as_u64()? as u8;
                let b = arr[2].as_u64()? as u8;
                Some((r, g, b))
            } else { None }
        });
    let rgb_hex = rgb_color.map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"));
    let supported_color_modes = attrs
        .get("supported_color_modes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let icon_name = match entity.domain.as_str() {
        "light"  => "lightbulb",
        "switch" => if value == "on" { "toggle-switch" } else { "toggle-switch-off" },
        "sensor" => match entity.device_class.as_deref() {
            Some("temperature")           => "thermometer",
            Some("humidity")              => "water-percent",
            Some("illuminance")           => "weather-sunny",
            Some("battery")               => "battery",
            Some("voltage")               => "lightning-bolt",
            Some("atmospheric_pressure")  => "gauge",
            // Source: homeassistant/components/sensor/__init__.py SensorDeviceClass icons
            Some("energy")                => "lightning-bolt",  // cumulative kWh counter
            Some("power")                 => "flash",           // instantaneous watts
            Some("current")               => "current-ac",      // RMS amperes
            _                             => "gauge",
        },
        "binary_sensor" => binary_sensor_icon(entity.device_class.as_deref().unwrap_or("")),
        _ => "home",
    };

    let service_action = match entity.domain.as_str() {
        "light" | "switch" => "toggle",
        _                  => "",
    };

    // Source: homeassistant/const.py STATE_UNAVAILABLE, STATE_UNKNOWN
    let is_unavailable = value == "unavailable" || value == "unknown";

    crate::entity_view::EntityView {
        entity_id:             entity.entity_id.clone(),
        webhook_id:            None, // Zigbee entities have no webhook registration
        display_name:          entity.display_name(),
        entity_type:           entity.domain.clone(),
        icon_name:             icon_name.to_string(),
        value,
        unit:                  entity.unit_of_measurement.clone().unwrap_or_default(),
        device_class:          entity.device_class.clone().unwrap_or_default(),
        user_area_id:          entity.user_area_id.clone().unwrap_or_default(),
        unit_of_measurement:   entity.unit_of_measurement.clone(),
        disabled:              false,
        service_action:        service_action.to_string(),
        current_temperature:   None,
        target_temperature:    None,
        hvac_modes:            vec![],
        preset_mode:           None,
        preset_modes:          vec![],
        fan_mode:              None,
        fan_modes:             vec![],
        target_humidity:       None,
        current_humidity:      None,
        brightness,
        color_temp_kelvin,
        min_color_temp_kelvin,
        max_color_temp_kelvin,
        color_mode,
        hs_color,
        rgb_color,
        rgb_hex,
        supported_color_modes,
        options:               vec![],
        current_position:      None,
        fan_percentage:        None,
        device_name,
        // Source: homeassistant/const.py STATE_UNAVAILABLE, STATE_UNKNOWN
        is_unavailable,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a minimal ZigbeeEntityRecord for testing.
    fn record(entity_id: &str, domain: &str, device_class: Option<&str>, unit: Option<&str>)
        -> ZigbeeEntityRecord
    {
        ZigbeeEntityRecord {
            entity_id: entity_id.to_string(),
            ieee_addr: "0x0000000000000001".to_string(),
            domain: domain.to_string(),
            attribute_key: Some(domain.to_string()),
            device_class: device_class.map(|s| s.to_string()),
            unit_of_measurement: unit.map(|s| s.to_string()),
            name_by_user: None,
            user_area_id: None,
        }
    }

    /// Verify entity_view_for assigns the correct icon for each device class.
    #[cfg(feature = "transport_wifi")]
    #[test]
    fn entity_view_for_icon_selection() {
        let states = StateStore::new();

        let cases: &[(&str, &str, Option<&str>, &str)] = &[
            ("light.bulb",                    "light",         None,                    "lightbulb"),
            ("sensor.temp",                   "sensor",        Some("temperature"),     "thermometer"),
            ("sensor.hum",                    "sensor",        Some("humidity"),        "water-percent"),
            ("sensor.lux",                    "sensor",        Some("illuminance"),     "weather-sunny"),
            ("sensor.bat",                    "sensor",        Some("battery"),         "battery"),
            ("sensor.volt",                   "sensor",        Some("voltage"),         "lightning-bolt"),
            ("sensor.press",                  "sensor",        Some("atmospheric_pressure"), "gauge"),
            ("sensor.other",                  "sensor",        None,                    "gauge"),
            // Source: homeassistant/components/sensor/__init__.py SensorDeviceClass icons
            ("sensor.energy",                 "sensor",        Some("energy"),          "lightning-bolt"),
            ("sensor.power",                  "sensor",        Some("power"),           "flash"),
            ("sensor.current",                "sensor",        Some("current"),         "current-ac"),
            // IAS zone type device classes — Source: homeassistant/components/binary_sensor/__init__.py
            ("binary_sensor.occupancy",       "binary_sensor", Some("occupancy"),       "motion-sensor"),
            ("binary_sensor.motion",          "binary_sensor", Some("motion"),          "motion-sensor"),
            ("binary_sensor.door",            "binary_sensor", Some("door"),            "door-closed"),
            ("binary_sensor.window",          "binary_sensor", Some("window"),          "door-closed"),
            ("binary_sensor.opening",         "binary_sensor", Some("opening"),         "door-closed"),
            ("binary_sensor.smoke",           "binary_sensor", Some("smoke"),           "smoke-detector"),
            ("binary_sensor.moisture",        "binary_sensor", Some("moisture"),        "water"),
            ("binary_sensor.co",              "binary_sensor", Some("carbon_monoxide"), "molecule-co"),
            ("binary_sensor.vibration",       "binary_sensor", Some("vibration"),       "vibrate"),
            ("binary_sensor.tamper",          "binary_sensor", Some("tamper"),          "shield-alert"),
            ("binary_sensor.bat_low",         "binary_sensor", Some("battery"),         "battery-alert"),
            ("binary_sensor.generic",         "binary_sensor", None,                    "alert-circle-outline"),
        ];

        for (entity_id, domain, device_class, expected_icon) in cases {
            let rec = record(entity_id, domain, *device_class, None);
            let view = entity_view_for(&rec, &states, None);
            assert_eq!(
                view.icon_name, *expected_icon,
                "entity={entity_id} domain={domain} class={device_class:?}"
            );
        }
    }

    /// Verify that push_state converts color_temp mireds to color_temp_kelvin.
    /// Source: homeassistant/components/light/__init__.py — kelvin = 1_000_000 / mireds.
    #[test]
    fn push_state_converts_color_temp_mireds_to_kelvin() {
        let ieee = "0x0000000000000002";
        let entities = vec![ZigbeeEntityRecord {
            entity_id: "light.test_bulb".to_string(),
            ieee_addr: ieee.to_string(),
            domain: "light".to_string(),
            attribute_key: None,
            device_class: None,
            unit_of_measurement: None,
            name_by_user: None,
            user_area_id: None,
        }];

        let mut raw = serde_json::Map::new();
        raw.insert("state".to_string(), serde_json::json!("ON"));
        raw.insert("brightness".to_string(), serde_json::json!(200));
        // 370 mireds ≈ 2703 K
        raw.insert("color_temp".to_string(), serde_json::json!(370));

        let state_store = StateStore::new();
        push_state(&raw, &entities, &state_store);

        let ha_state = state_store.get("light.test_bulb")
            .expect("light state should be set after push_state");
        assert_eq!(ha_state.state, "on");
        assert_eq!(ha_state.attributes.get("brightness"), Some(&serde_json::json!(200)));
        let kelvin = ha_state.attributes.get("color_temp_kelvin")
            .and_then(|v| v.as_u64())
            .expect("color_temp_kelvin attribute must be present");
        // round(1_000_000 / 370) = 2703
        assert_eq!(kelvin, 2703, "expected 2703 K for 370 mireds");
        // Original mireds value must also be preserved
        let mireds = ha_state.attributes.get("color_temp")
            .and_then(|v| v.as_f64())
            .expect("color_temp (mireds) attribute must be preserved");
        assert!((mireds - 370.0).abs() < 0.001);
    }

    /// entity_view_for sets webhook_id to None for Zigbee entities.
    #[cfg(feature = "transport_wifi")]
    #[test]
    fn entity_view_for_webhook_id_is_none_for_zigbee() {
        let states = StateStore::new();
        let rec = record("sensor.test_temperature", "sensor", Some("temperature"), Some("°C"));
        let view = entity_view_for(&rec, &states, None);
        assert!(view.webhook_id.is_none(), "Zigbee entities must not have a webhook_id");
    }

    /// ias_device_class maps ZCL zone type values to HA device classes.
    ///
    /// Source: homeassistant/components/zha/sensor.py IAS_ZONE_TYPE_MAP
    /// Source: homeassistant/components/binary_sensor/__init__.py BinarySensorDeviceClass
    #[test]
    fn ias_device_class_mapping() {
        assert_eq!(ias_device_class(Some(0x000d)), "motion");
        assert_eq!(ias_device_class(Some(0x0015)), "door");
        assert_eq!(ias_device_class(Some(0x0028)), "smoke");
        assert_eq!(ias_device_class(Some(0x002a)), "moisture");
        assert_eq!(ias_device_class(Some(0x002b)), "carbon_monoxide");
        assert_eq!(ias_device_class(Some(0x002c)), "safety");
        assert_eq!(ias_device_class(Some(0x002d)), "vibration");
        // Unknown / not-yet-reported type falls back to "opening"
        assert_eq!(ias_device_class(Some(0x010f)), "opening");
        assert_eq!(ias_device_class(None),          "opening");
    }

    // Build a minimal DeviceInfo for unit tests.
    fn make_device_info(ieee_hex: &str, name: &str, clusters: &[u16]) -> zigbee2mqtt_rs::DeviceInfo {
        zigbee2mqtt_rs::DeviceInfo {
            ieee_addr:     zigbee2mqtt_rs::IeeeAddr::from_hex(ieee_hex).unwrap(),
            nwk_addr:      0x1234,
            friendly_name: name.to_string(),
            manufacturer:  None,
            model:         None,
            power_source:  None,
            sw_build_id:   None,
            endpoints:     vec![zigbee2mqtt_rs::EndpointDesc {
                endpoint:        1,
                profile_id:      0x0104,
                device_id:       0x0402,
                input_clusters:  clusters.to_vec(),
                output_clusters: vec![],
            }],
            initial_state: serde_json::Map::new(),
        }
    }

    /// entities_for_device uses zone_type from initial_state to set device_class.
    #[test]
    fn entities_for_device_ias_zone_type_motion() {
        let mut info = make_device_info("0xAABBCCDDEEFF0011", "pir_sensor", &[0x0500]);
        info.initial_state.insert("zone_type".to_string(), serde_json::json!(0x000du16));

        let records = entities_for_device(&info);
        let contact = records.iter().find(|r| r.attribute_key.as_deref() == Some("contact"));
        assert!(contact.is_some(), "contact entity must exist for IAS device");
        assert_eq!(
            contact.unwrap().device_class.as_deref(),
            Some("motion"),
            "zone_type 0x000d must map to device_class 'motion'"
        );
    }

    #[test]
    fn entities_for_device_ias_zone_type_fallback() {
        // No zone_type in initial_state → fallback to "opening"
        let info = make_device_info("0xAABBCCDDEEFF0022", "unknown_sensor", &[0x0500]);

        let records = entities_for_device(&info);
        let contact = records.iter().find(|r| r.attribute_key.as_deref() == Some("contact"));
        assert_eq!(
            contact.unwrap().device_class.as_deref(),
            Some("opening"),
            "missing zone_type must fall back to 'opening'"
        );
    }
}
