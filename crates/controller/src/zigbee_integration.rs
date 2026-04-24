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
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use zigbee2mqtt_rs::bridge::Bridge;
use zigbee2mqtt_rs::config::{AdapterType, AdvancedConfig, Config, SerialConfig};
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
}

// ---------------------------------------------------------------------------
// Cluster → entity mapping (mirrors zigbee2mqtt-rs/src/homeassistant.rs)
// ---------------------------------------------------------------------------

/// Derive HA entity records from a device's ZCL input clusters.
///
/// Mirrors the discovery logic in `zigbee2mqtt-rs/src/homeassistant.rs` so
/// that entity domains/classes are consistent with what zigbee2mqtt would
/// expose via MQTT, but without requiring a broker.
pub fn entities_for_device(device: &zigbee2mqtt_rs::Device) -> Vec<ZigbeeEntityRecord> {
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
        // IAS Zone (0x0500) emits "contact" (door/window sensors),
        // "tamper", and "battery_low".  Device class "door" is the
        // most common ZHA default; the user can rename if needed.
        records.push(ZigbeeEntityRecord {
            entity_id: format!("binary_sensor.{base}_contact"),
            ieee_addr: ieee.clone(),
            domain: "binary_sensor".to_string(),
            attribute_key: Some("contact".to_string()),
            device_class: Some("door".to_string()),
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
                // HA uses state_class = "measurement" for numeric sensors to enable
                // long-term statistics and the history graph sparkline.
                attrs.insert("state_class".into(), Value::String("measurement".into()));
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
            }
            _ => {}
        }

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

/// Consume events from `event_rx` and fan them out to the stores.
///
/// This is the inner loop extracted so it can be driven from tests without
/// spinning up a real `Bridge`. Call it inside a `tokio::spawn` block.
pub async fn run_event_loop(
    mut event_rx: mpsc::Receiver<ZigbeeEvent>,
    device_store: Arc<ZigbeeDeviceStore>,
    entity_store: Arc<ZigbeeEntityStore>,
    state_store: Arc<StateStore>,
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

            ZigbeeEvent::DeviceInterviewComplete { ieee_addr, device } => {
                let ieee = ieee_addr.as_hex();
                info!("Zigbee interview complete: {ieee} model={:?}", device.model);

                // Upsert full device record.
                let record = ZigbeeDeviceRecord {
                    ieee_addr: ieee.clone(),
                    friendly_name: device.friendly_name.clone(),
                    manufacturer: device.manufacturer.clone(),
                    model: device.model.clone(),
                    power_source: device.power_source.clone(),
                    sw_build_id: device.sw_build_id.clone(),
                    interview_complete: true,
                    last_seen: None,
                    name_by_user: None,
                    user_area_id: None,
                };
                if let Err(e) = device_store.upsert(record).await {
                    warn!("zigbee device store upsert failed: {e:#}");
                }

                // Derive and register entities.
                let entity_records = entities_for_device(&device);
                if let Err(e) = entity_store.register_bulk(entity_records.clone()).await {
                    warn!("zigbee entity store register failed: {e:#}");
                }

                // Push initial state from the device's cached values.
                push_state(&device.state, &entity_records, &state_store);
            }

            ZigbeeEvent::StateChanged { ieee_addr, state } => {
                let ieee = ieee_addr.as_hex();
                if let Ok(entities) = entity_store.list_for_device(&ieee).await {
                    push_state(&state, &entities, &state_store);
                }
                // Record freshness — mirrors HA's last_seen tracking on state updates.
                let ts = now_iso8601();
                let _ = device_store.touch_last_seen(&ieee, ts).await;
            }
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
        mqtt: None, // no broker — events come via the notify channel
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
    let (bridge, event_rx, cmd_tx) = Bridge::start(z2m_cfg, config_path);

    // Spawn the bridge task.
    tokio::spawn(async move {
        if let Err(e) = bridge.run().await {
            error!("Zigbee bridge exited with error: {e:#}");
        }
    });

    // Spawn the event fan-out task.
    tokio::spawn(run_event_loop(event_rx, device_store, entity_store, state_store));

    ZigbeeHandle {
        cmd_tx,
        permit_join_expires_at: Arc::new(AtomicU64::new(0)),
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
            _                             => "gauge",
        },
        "binary_sensor" => match entity.device_class.as_deref() {
            Some("occupancy") | Some("motion") => "motion-sensor",
            Some("door") | Some("window")      => "door",
            Some("tamper")                     => "shield-alert",
            Some("battery")                    => "battery-alert",
            _                                  => "radiobox-marked",
        },
        _ => "home",
    };

    let service_action = match entity.domain.as_str() {
        "light" | "switch" => "toggle",
        _                  => "",
    };

    crate::entity_view::EntityView {
        entity_id:             entity.entity_id.clone(),
        webhook_id:            None, // Zigbee entities have no webhook registration
        display_name:          entity.display_name().to_string(),
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
        brightness,
        color_temp_kelvin,
        min_color_temp_kelvin,
        max_color_temp_kelvin,
        options:               vec![],
        current_position:      None,
        fan_percentage:        None,
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
            ("light.bulb",                  "light",         None,                  "lightbulb"),
            ("sensor.temp",                 "sensor",        Some("temperature"),   "thermometer"),
            ("sensor.hum",                  "sensor",        Some("humidity"),      "water-percent"),
            ("sensor.lux",                  "sensor",        Some("illuminance"),   "weather-sunny"),
            ("sensor.bat",                  "sensor",        Some("battery"),       "battery"),
            ("sensor.volt",                 "sensor",        Some("voltage"),       "lightning-bolt"),
            ("sensor.press",                "sensor",        Some("atmospheric_pressure"), "gauge"),
            ("sensor.other",                "sensor",        None,                  "gauge"),
            ("binary_sensor.occupancy",     "binary_sensor", Some("occupancy"),     "motion-sensor"),
            ("binary_sensor.motion",        "binary_sensor", Some("motion"),        "motion-sensor"),
            ("binary_sensor.door",          "binary_sensor", Some("door"),          "door"),
            ("binary_sensor.window",        "binary_sensor", Some("window"),        "door"),
            ("binary_sensor.tamper",        "binary_sensor", Some("tamper"),        "shield-alert"),
            ("binary_sensor.bat_low",       "binary_sensor", Some("battery"),       "battery-alert"),
            ("binary_sensor.generic",       "binary_sensor", None,                  "radiobox-marked"),
        ];

        for (entity_id, domain, device_class, expected_icon) in cases {
            let rec = record(entity_id, domain, *device_class, None);
            let view = entity_view_for(&rec, &states);
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
        let view = entity_view_for(&rec, &states);
        assert!(view.webhook_id.is_none(), "Zigbee entities must not have a webhook_id");
    }
}
