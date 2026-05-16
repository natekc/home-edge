use anyhow::Result;

#[cfg(feature = "transport_wifi")]
use anyhow::Context;
#[cfg(feature = "transport_ble")]
use anyhow::bail;

use crate::config::AppConfig;
use crate::core::AppCore;
#[cfg(feature = "transport_wifi")]
use crate::core::RuntimeMode;

#[cfg(feature = "transport_wifi")]
use std::sync::Arc;
#[cfg(feature = "transport_wifi")]
use tokio::signal;
#[cfg(feature = "transport_wifi")]
use tracing::info;

#[cfg(feature = "transport_wifi")]
use crate::area_registry_store::AreaRegistryStore;
#[cfg(feature = "transport_wifi")]
use crate::label_registry_store::LabelRegistryStore;
#[cfg(feature = "transport_wifi")]
use crate::notification_store::NotificationStore;
use crate::zone_store::ZoneStore;
#[cfg(feature = "transport_wifi")]
use crate::person_store::PersonStore;
#[cfg(feature = "transport_wifi")]
use crate::auth_store::AuthStore;
#[cfg(feature = "transport_wifi")]
use crate::long_lived_token_store::LongLivedTokenStore;
#[cfg(feature = "transport_wifi")]
use crate::ha_auth::{LoginFlowStore, TokenStore};
#[cfg(feature = "transport_wifi")]
use crate::ha_webhook::WebhookStore;
#[cfg(feature = "transport_wifi")]
use crate::http;
#[cfg(feature = "transport_wifi")]
use crate::mobile_device_store::MobileDeviceStore;
#[cfg(feature = "transport_wifi")]
use crate::mobile_entity_store::MobileEntityStore;
#[cfg(feature = "transport_wifi")]
use crate::service::ServiceRegistry;
#[cfg(feature = "transport_wifi")]
use crate::state_store::StateStore;
#[cfg(feature = "transport_wifi")]
use crate::storage::Storage;
#[cfg(feature = "transport_wifi")]
use crate::zeroconf;
#[cfg(feature = "transport_wifi")]
use tokio::sync::broadcast::error::RecvError;
#[cfg(all(feature = "transport_wifi", feature = "zigbee"))]
use crate::zigbee_device_store::ZigbeeDeviceStore;
#[cfg(feature = "zigbee")]
use crate::zigbee_entity_store::ZigbeeEntityStore;
#[cfg(feature = "zigbee")]
use crate::zigbee_integration::ZigbeeHandle;

#[cfg(feature = "transport_wifi")]
pub struct AppState {
    pub core: AppCore,
    pub config: AppConfig,
    pub storage: Storage,
    pub auth: AuthStore,
    pub area_registry: AreaRegistryStore,
    /// Source: homeassistant/helpers/label_registry.py  LabelRegistry
    pub label_registry: LabelRegistryStore,
    pub zone_store: ZoneStore,
    /// Person entity registry and location state.
    /// Source: homeassistant/components/person/__init__.py  PersonStorageCollection
    pub person_store: Arc<PersonStore>,
    pub mobile_devices: MobileDeviceStore,
    pub mobile_entities: MobileEntityStore,
    pub states: std::sync::Arc<StateStore>,
    pub tokens: TokenStore,
    pub flows: LoginFlowStore,
    pub webhooks: WebhookStore,
    pub services: ServiceRegistry,
    pub templates: minijinja::Environment<'static>,
    pub history: std::sync::Arc<crate::history_store::HistoryStore>,
    pub long_lived_tokens: LongLivedTokenStore,
    /// Unix timestamp (seconds) of process start — used for uptime display.
    /// Source: homeassistant/components/system_health/__init__.py  SystemHealthInfo
    pub start_time: std::time::Instant,
    pub logbook: std::sync::Arc<crate::logbook_store::LogbookStore>,
    pub notifications: NotificationStore,
    /// Zigbee device registry (only populated when the `zigbee` feature is enabled
    /// and `[zigbee]` is present in config.toml).
    #[cfg(feature = "zigbee")]
    pub zigbee_devices: Arc<ZigbeeDeviceStore>,
    #[cfg(feature = "zigbee")]
    pub zigbee_entities: Arc<ZigbeeEntityStore>,
    /// Operational handle for the running zigbee bridge; `None` if the
    /// `zigbee` feature is disabled or `[zigbee]` is absent from config.
    #[cfg(feature = "zigbee")]
    pub zigbee: Option<ZigbeeHandle>,
}

#[cfg(feature = "transport_wifi")]
impl AppState {
    pub fn new(config: AppConfig, storage: Storage) -> Self {
        let auth = AuthStore::new(storage.root().to_path_buf());
        let area_registry = AreaRegistryStore::new(storage.root().to_path_buf());
        let label_registry = LabelRegistryStore::new(storage.root().to_path_buf());
        let zone_store = ZoneStore::new(storage.root().to_path_buf());
        // Source: homeassistant/components/person/__init__.py  PersonStorageCollection
        let person_store = Arc::new(PersonStore::new());
        let mobile_devices = MobileDeviceStore::new(storage.root().to_path_buf());
        let mobile_entities = MobileEntityStore::new(storage.root().to_path_buf());
        let tokens = TokenStore::new(storage.root().to_path_buf());
        let long_lived_tokens = LongLivedTokenStore::new(storage.root().to_path_buf());
        let history_capacity = config.history.capacity;
        let history_db_path = storage.root().join("history.db");
        #[cfg(feature = "zigbee")]
        let zigbee_devices = Arc::new(ZigbeeDeviceStore::new(storage.root().to_path_buf()));
        #[cfg(feature = "zigbee")]
        let zigbee_entities = Arc::new(ZigbeeEntityStore::new(storage.root().to_path_buf()));
        Self {
            core: AppCore::new(),
            auth,
            area_registry,
            label_registry,
            zone_store,
            person_store,
            mobile_devices,
            mobile_entities,
            config,
            storage,
            states: std::sync::Arc::new(StateStore::new()),
            tokens,
            flows: LoginFlowStore::new(),
            webhooks: WebhookStore::new(),
            services: ServiceRegistry::new(),
            templates: crate::templates::build_env(),
            history: std::sync::Arc::new(
                crate::history_store::HistoryStore::open(history_capacity, &history_db_path)
            ),
            long_lived_tokens,
            start_time: std::time::Instant::now(),
            logbook: std::sync::Arc::new(crate::logbook_store::LogbookStore::new(history_capacity)),
            notifications: NotificationStore::new(),
            #[cfg(feature = "zigbee")]
            zigbee_devices,
            #[cfg(feature = "zigbee")]
            zigbee_entities,
            #[cfg(feature = "zigbee")]
            zigbee: None,
        }
    }

    pub async fn new_initialized(config: AppConfig, storage: Storage) -> Result<Self> {
        let mut state = Self::new(config, storage);
        let onboarding = state.storage.load_onboarding().await?;
        state
            .core
            .set_runtime_mode(RuntimeMode::from_persisted_onboarding(onboarding.onboarded));
        state.tokens.load_persisted().await?;
        state.long_lived_tokens.load().await?;
        state
            .area_registry
            .seed_if_empty(&state.config.areas.names)
            .await?;
        // Seed the home zone coordinates from config.toml on first boot.
        // If OnboardingState already has coordinates (set during or after onboarding)
        // this is a no-op — matching the same once-only-seed pattern as [areas].
        state
            .storage
            .seed_home_zone_coords_if_unset(
                state.config.home_zone.latitude,
                state.config.home_zone.longitude,
                state.config.home_zone.radius,
            )
            .await?;
        // Start Zigbee bridge if configured.
        #[cfg(feature = "zigbee")]
        if let Some(ref zigbee_cfg) = state.config.zigbee.clone() {
            let handle = crate::zigbee_integration::start(
                zigbee_cfg.clone(),
                Arc::clone(&state.zigbee_devices),
                Arc::clone(&state.zigbee_entities),
                Arc::clone(&state.states),
                Arc::clone(&state.history),
                Arc::clone(&state.logbook),
            ).await;
            state.zigbee = Some(handle);
            tracing::info!("Zigbee bridge started");

            // Re-seed the in-memory StateStore from persisted history so
            // entities don't show "unavailable" after a redeploy until the
            // sensor next transmits.  Mirrors HA's "restore last known state"
            // behaviour (homeassistant/helpers/restore_state.py).
            crate::zigbee_integration::restore_states_from_history(
                &state.zigbee_entities,
                &state.history,
                &state.states,
            ).await;
        }
        Ok(state)
    }

    /// Render a named minijinja template and return the HTML string.
    pub fn render_html(
        &self,
        name: &str,
        ctx: minijinja::Value,
    ) -> Result<String, minijinja::Error> {
        self.templates.get_template(name)?.render(ctx)
    }
}

#[cfg(feature = "transport_wifi")]
pub async fn run(config: AppConfig, reset: bool, demo: bool) -> Result<()> {
    let listen_addr = config.listen_addr();

    if reset {
        let data_dir = &config.storage.data_dir;
        if data_dir.exists() {
            tokio::fs::remove_dir_all(data_dir)
                .await
                .with_context(|| format!("failed to wipe data dir {}", data_dir.display()))?;
            tracing::warn!(path = %data_dir.display(), "--reset: wiped data directory");
        } else {
            tracing::info!("--reset: data directory does not exist, nothing to wipe");
        }
    }

    let storage = Storage::new(config.storage.data_dir.clone()).await?;
    let state = Arc::new(AppState::new_initialized(config, storage).await?);

    if demo {
        seed_demo(&state).await?;
    }

    // Spawn logbook listener: subscribes to StateStore broadcast and records entries.
    // state.clone() increments the Arc reference count — the spawned task needs its
    // own owned Arc handle since tokio::spawn requires 'static.
    {
        let state_clone = state.clone();
        let mut rx = state.states.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let old = event
                            .old_state
                            .as_ref()
                            .map(|s| s.state.as_str())
                            .unwrap_or("")
                            .to_string();
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .expect("system clock is before UNIX epoch")
                            .as_secs();
                        let display_name = event
                            .state
                            .attributes
                            .get("friendly_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&event.state.entity_id)
                            .to_string();
                        let entry = crate::logbook_store::LogbookEntry {
                            ts,
                            entity_id: event.state.entity_id.clone(),
                            display_name,
                            old_state: old,
                            new_state: event.state.state.clone(),
                        };
                        state_clone.logbook.record(entry).await;
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "logbook listener lagged, {n} events dropped");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    let _zeroconf = zeroconf::announce(&state).await?;

    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;

    info!(address = %listen_addr, "home edge listening");

    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server exited unexpectedly")?;

    Ok(())
}

#[cfg(feature = "transport_ble")]
pub async fn run(_config: AppConfig, _reset: bool, _demo: bool) -> Result<()> {
    let _core = AppCore::new();
    bail!("BLE transport build is not implemented yet")
}

#[cfg(feature = "transport_ble")]
pub async fn run(_config: AppConfig, _reset: bool, _demo: bool) -> Result<()> {
    let _core = AppCore::new();
    bail!("BLE transport build is not implemented yet")
}

/// Seed realistic demo data for `--demo` / `cargo xtask screenshot`.
///
/// Populates the in-memory stores with two Zigbee devices, a mobile device,
/// three areas, and synthetic sensor history so every UI page renders with
/// believable content instead of empty-state placeholders.
#[cfg(feature = "transport_wifi")]
async fn seed_demo(state: &AppState) -> Result<()> {
    use crate::mobile_device_store::MobileDeviceRegistration;
    use crate::mobile_entity_store::MobileEntityRegistration;
    use crate::storage::OnboardingState;
    #[cfg(feature = "zigbee")]
    use crate::zigbee_device_store::ZigbeeDeviceRecord;
    #[cfg(feature = "zigbee")]
    use crate::zigbee_entity_store::ZigbeeEntityRecord;

    tracing::info!("--demo: seeding demo data");

    // ── Onboarding ─────────────────────────────────────────────────────────
    // Mark onboarding complete so the server goes straight to the dashboard.
    let onboarding = OnboardingState {
        onboarded: true,
        updated_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        done: vec![
            "user".into(),
            "core_config".into(),
            "analytics".into(),
            "integration".into(),
        ],
        user: Some(crate::storage::StoredUser {
            name:     "Nathan".into(),
            username: "nathan".into(),
            password: crate::auth_store::hash_password("demo")
                .unwrap_or_else(|_| "demo".into()),
            language: "en".into(),
        }),
        location_name: Some("Nathan's Home".into()),
        latitude:   None,
        longitude:  None,
        country:    Some("US".into()),
        language:   Some("en".into()),
        time_zone:  Some("America/Los_Angeles".into()),
        unit_system: Some("metric".into()),
        radius: 100.0,
        version: 1,
    };
    state.storage.save_onboarding(&onboarding).await?;
    state.core.set_runtime_mode(crate::core::RuntimeMode::from_persisted_onboarding(true));

    // ── Areas ──────────────────────────────────────────────────────────────
    let living_room = state.area_registry.create("Living Room".into()).await?;
    let bedroom     = state.area_registry.create("Bedroom".into()).await?;
    let office      = state.area_registry.create("Office".into()).await?;

    // ── Mobile device (iPhone) ─────────────────────────────────────────────
    let phone = state.mobile_devices.register(MobileDeviceRegistration {
        app_id:              "io.home-assistant.Home-Assistant".into(),
        app_name:            "Home Assistant".into(),
        app_version:         "2025.1.0".into(),
        device_name:         "Nathan's iPhone".into(),
        manufacturer:        "Apple".into(),
        model:               "iPhone 16 Pro".into(),
        os_name:             "iOS".into(),
        os_version:          Some("18.2".into()),
        device_id:           Some("demo-iphone-001".into()),
        supports_encryption: false,
        owner_username:      Some("nathan".into()),
    }).await?;
    let wid = phone.webhook_id.clone();

    // Mobile entities: battery, step counter, temperature, WiFi signal
    for (name, etype, dc, unit, uid) in [
        ("Battery Level",  "sensor",        Some("battery"),     Some("%"),  "battery"),
        ("Steps",          "sensor",        None,                None,       "steps"),
        ("CPU Temperature","sensor",        Some("temperature"), Some("°C"), "cpu_temp"),
        ("Storage",        "sensor",        Some("data_size"),   Some("GB"), "storage"),
    ] {
        let reg = MobileEntityRegistration {
            webhook_id:         wid.clone(),
            entity_type:        etype.into(),
            sensor_unique_id:   format!("demo-iphone-{uid}"),
            sensor_name:        name.into(),
            device_class:       dc.map(|s| s.into()),
            unit_of_measurement: unit.map(|s| s.into()),
            icon:               None,
            entity_category:    None,
            state_class:        Some("measurement".into()),
            disabled:           false,
        };
        state.mobile_entities.register(reg).await?;
    }

    // Seed entity states for mobile entities
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ts_iso = crate::state_store::now_iso8601();

    let mobile_states: &[(&str, &str)] = &[
        (&format!("sensor.nathans_iphone_battery_level"), "82"),
        (&format!("sensor.nathans_iphone_steps"), "4231"),
        (&format!("sensor.nathans_iphone_cpu_temperature"), "38"),
        (&format!("sensor.nathans_iphone_storage"), "47.3"),
    ];
    for (entity_id, value) in mobile_states {
        let _ = state.states.set(ha_types::entity::State {
            entity_id: entity_id.to_string(),
            state:     value.to_string(),
            attributes: Default::default(),
            last_changed: ts_iso.clone(),
            last_updated: ts_iso.clone(),
            last_reported: ts_iso.clone(),
            context: ha_types::context::Context::new(uuid::Uuid::new_v4().to_string()),
        });
        if let Ok(v) = value.parse::<f64>() {
            state.history.record_and_aggregate(entity_id, v, now_ts).await;
        }
    }

    // Assign mobile entities to areas
    let all_mobile = state.mobile_entities.all().await?;
    for ent in &all_mobile {
        let _ = state.mobile_entities.update_meta(&ent.entity_id, crate::mobile_entity_store::EntityMetaUpdate {
            name_by_user:        None,
            user_area_id:        Some(Some(living_room.area_id.clone())),
            unit_of_measurement: None,
            disabled:            None,
            hidden_by:           None,
            icon:                None,
            labels:              None,
        }).await;
    }

    // ── Zigbee: SNZB-02 temperature/humidity sensor ────────────────────────
    #[cfg(feature = "zigbee")]
    {
        let snzb_ieee  = "0xec1bbdfffecafe01".to_string();
        let bulb_ieee  = "0xec1bbdfffecafe02".to_string();
        let pir_ieee   = "0xec1bbdfffecafe03".to_string();

        // Device 1: SONOFF SNZB-02 (temp + humidity sensor)
        state.zigbee_devices.upsert(ZigbeeDeviceRecord {
            ieee_addr:         snzb_ieee.clone(),
            friendly_name:     "snzb_02_bedroom".into(),
            manufacturer:      Some("SONOFF".into()),
            model:             Some("SNZB-02".into()),
            power_source:      Some("Battery".into()),
            sw_build_id:       Some("1.0.4".into()),
            interview_complete: true,
            last_seen:         Some("2026-05-15T08:32:11Z".into()),
            name_by_user:      Some("Bedroom Sensor".into()),
            user_area_id:      Some(bedroom.area_id.clone()),
        }).await?;

        // Device 2: IKEA Tradfri bulb
        state.zigbee_devices.upsert(ZigbeeDeviceRecord {
            ieee_addr:         bulb_ieee.clone(),
            friendly_name:     "tradfri_living_room".into(),
            manufacturer:      Some("IKEA of Sweden".into()),
            model:             Some("LED1624G9".into()),
            power_source:      Some("Mains (single phase)".into()),
            sw_build_id:       Some("2.3.087".into()),
            interview_complete: true,
            last_seen:         Some("2026-05-15T08:45:00Z".into()),
            name_by_user:      Some("Living Room Bulb".into()),
            user_area_id:      Some(living_room.area_id.clone()),
        }).await?;

        // Device 3: SONOFF SNZB-03 PIR motion sensor
        state.zigbee_devices.upsert(ZigbeeDeviceRecord {
            ieee_addr:         pir_ieee.clone(),
            friendly_name:     "snzb_03_office".into(),
            manufacturer:      Some("SONOFF".into()),
            model:             Some("SNZB-03".into()),
            power_source:      Some("Battery".into()),
            sw_build_id:       Some("1.0.2".into()),
            interview_complete: true,
            last_seen:         Some("2026-05-15T08:40:00Z".into()),
            name_by_user:      Some("Office Motion".into()),
            user_area_id:      Some(office.area_id.clone()),
        }).await?;

        // Entities for SNZB-02
        let snzb_entities = vec![
            ZigbeeEntityRecord {
                entity_id:           "sensor.snzb_02_bedroom_temperature".into(),
                ieee_addr:            snzb_ieee.clone(),
                domain:               "sensor".into(),
                attribute_key:        Some("temperature".into()),
                device_class:         Some("temperature".into()),
                unit_of_measurement:  Some("°C".into()),
                name_by_user:         None,
                user_area_id:         Some(bedroom.area_id.clone()),
                disabled:             false,
                icon:                 None,
                hidden_by:            None,
                labels:               vec![],
            },
            ZigbeeEntityRecord {
                entity_id:           "sensor.snzb_02_bedroom_humidity".into(),
                ieee_addr:            snzb_ieee.clone(),
                domain:               "sensor".into(),
                attribute_key:        Some("humidity".into()),
                device_class:         Some("humidity".into()),
                unit_of_measurement:  Some("%".into()),
                name_by_user:         None,
                user_area_id:         Some(bedroom.area_id.clone()),
                disabled:             false,
                icon:                 None,
                hidden_by:            None,
                labels:               vec![],
            },
            ZigbeeEntityRecord {
                entity_id:           "sensor.snzb_02_bedroom_battery".into(),
                ieee_addr:            snzb_ieee.clone(),
                domain:               "sensor".into(),
                attribute_key:        Some("battery".into()),
                device_class:         Some("battery".into()),
                unit_of_measurement:  Some("%".into()),
                name_by_user:         None,
                user_area_id:         None,
                disabled:             false,
                icon:                 None,
                hidden_by:            None,
                labels:               vec![],
            },
        ];
        state.zigbee_entities.register_bulk(snzb_entities).await?;

        // Entities for IKEA bulb
        let bulb_entities = vec![
            ZigbeeEntityRecord {
                entity_id:           "light.tradfri_living_room".into(),
                ieee_addr:            bulb_ieee.clone(),
                domain:               "light".into(),
                attribute_key:        None,
                device_class:         None,
                unit_of_measurement:  None,
                name_by_user:         Some("Living Room Bulb".into()),
                user_area_id:         Some(living_room.area_id.clone()),
                disabled:             false,
                icon:                 None,
                hidden_by:            None,
                labels:               vec![],
            },
        ];
        state.zigbee_entities.register_bulk(bulb_entities).await?;

        // Entities for SNZB-03 PIR
        let pir_entities = vec![
            ZigbeeEntityRecord {
                entity_id:           "binary_sensor.snzb_03_office_occupancy".into(),
                ieee_addr:            pir_ieee.clone(),
                domain:               "binary_sensor".into(),
                attribute_key:        Some("occupancy".into()),
                device_class:         Some("occupancy".into()),
                unit_of_measurement:  None,
                name_by_user:         None,
                user_area_id:         Some(office.area_id.clone()),
                disabled:             false,
                icon:                 None,
                hidden_by:            None,
                labels:               vec![],
            },
            ZigbeeEntityRecord {
                entity_id:           "sensor.snzb_03_office_battery".into(),
                ieee_addr:            pir_ieee.clone(),
                domain:               "sensor".into(),
                attribute_key:        Some("battery".into()),
                device_class:         Some("battery".into()),
                unit_of_measurement:  Some("%".into()),
                name_by_user:         None,
                user_area_id:         None,
                disabled:             false,
                icon:                 None,
                hidden_by:            None,
                labels:               vec![],
            },
        ];
        state.zigbee_entities.register_bulk(pir_entities).await?;

        // Push Zigbee entity states
        let zigbee_states: &[(&str, &str)] = &[
            ("sensor.snzb_02_bedroom_temperature",    "21.4"),
            ("sensor.snzb_02_bedroom_humidity",       "58"),
            ("sensor.snzb_02_bedroom_battery",        "82"),
            ("light.tradfri_living_room",             "on"),
            ("binary_sensor.snzb_03_office_occupancy","on"),
            ("sensor.snzb_03_office_battery",         "64"),
        ];
        for (entity_id, value) in zigbee_states {
            let _ = state.states.set(ha_types::entity::State {
                entity_id:    entity_id.to_string(),
                state:        value.to_string(),
                attributes:   Default::default(),
                last_changed: ts_iso.clone(),
                last_updated: ts_iso.clone(),
                last_reported: ts_iso.clone(),
                context: ha_types::context::Context::new(uuid::Uuid::new_v4().to_string()),
            });
        }

        // Seed 24h of synthetic temperature history (sine wave-ish)
        let period_secs: u64 = 86_400;
        let base_ts = now_ts.saturating_sub(period_secs);
        for i in 0u64..=48 {
            let t = base_ts + i * (period_secs / 48);
            let v = 20.0 + 3.0 * (std::f64::consts::TAU * i as f64 / 48.0).sin();
            state.history.record_and_aggregate("sensor.snzb_02_bedroom_temperature", v, t).await;
        }
        for i in 0u64..=48 {
            let t = base_ts + i * (period_secs / 48);
            let v = 55.0 + 8.0 * (std::f64::consts::TAU * i as f64 / 48.0 + 1.0).cos();
            state.history.record_and_aggregate("sensor.snzb_02_bedroom_humidity", v, t).await;
        }
        // Binary sensor history (alternating motion events)
        for i in 0u64..=24 {
            let t = base_ts + i * (period_secs / 24);
            let v = if i % 3 == 0 { 1.0 } else { 0.0 };
            state.history.record_and_aggregate("binary_sensor.snzb_03_office_occupancy", v, t).await;
        }
    }

    // ── Notifications ──────────────────────────────────────────────────────
    state.notifications.create(
        "Motion detected in Office at 08:40".into(),
        Some("Motion Alert".into()),
        None,
    ).await;
    state.notifications.create(
        "Battery low on Bedroom Sensor (12%)".into(),
        Some("Low Battery".into()),
        None,
    ).await;

    // ── Logbook entries ────────────────────────────────────────────────────
    // Source: homeassistant/components/logbook/ — state-change event format
    {
        use crate::logbook_store::LogbookEntry;
        let now = now_ts;
        let entries = vec![
            LogbookEntry { ts: now - 3600, entity_id: "sensor.snzb_02_bedroom_temperature".into(), display_name: "Bedroom Temperature".into(), old_state: "19.8".into(), new_state: "20.1".into() },
            LogbookEntry { ts: now - 3200, entity_id: "light.tradfri_bulb_bedroom".into(), display_name: "Bedroom Bulb".into(), old_state: "off".into(), new_state: "on".into() },
            LogbookEntry { ts: now - 2800, entity_id: "sensor.snzb_02_bedroom_humidity".into(), display_name: "Bedroom Humidity".into(), old_state: "54".into(), new_state: "56".into() },
            LogbookEntry { ts: now - 2400, entity_id: "binary_sensor.snzb_03_office_occupancy".into(), display_name: "Office Motion".into(), old_state: "off".into(), new_state: "on".into() },
            LogbookEntry { ts: now - 2000, entity_id: "binary_sensor.snzb_03_office_occupancy".into(), display_name: "Office Motion".into(), old_state: "on".into(), new_state: "off".into() },
            LogbookEntry { ts: now - 1600, entity_id: "sensor.snzb_02_bedroom_temperature".into(), display_name: "Bedroom Temperature".into(), old_state: "20.1".into(), new_state: "20.4".into() },
            LogbookEntry { ts: now - 1200, entity_id: "light.tradfri_bulb_bedroom".into(), display_name: "Bedroom Bulb".into(), old_state: "on".into(), new_state: "off".into() },
            LogbookEntry { ts: now - 800,  entity_id: "sensor.phone_battery_level".into(), display_name: "Phone Battery".into(), old_state: "90".into(), new_state: "84".into() },
            LogbookEntry { ts: now - 400,  entity_id: "sensor.snzb_02_bedroom_temperature".into(), display_name: "Bedroom Temperature".into(), old_state: "20.4".into(), new_state: "20.2".into() },
            LogbookEntry { ts: now - 120,  entity_id: "binary_sensor.snzb_03_office_occupancy".into(), display_name: "Office Motion".into(), old_state: "off".into(), new_state: "on".into() },
        ];
        for entry in entries {
            state.logbook.record(entry).await;
        }
    }

    tracing::info!("--demo: seeding complete");
    Ok(())
}

#[cfg(feature = "transport_wifi")]
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut signal = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let _ = signal.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(all(test, feature = "transport_wifi"))]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;

    use super::*;
    use crate::config::{AreasConfig, ServerConfig, StorageConfig, UiConfig};
    use crate::storage::{OnboardingState, Storage};

    fn test_config() -> AppConfig {
        AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                log_level: tracing::Level::INFO,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/home-edge-app-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
            areas: AreasConfig::default(),
            home_zone: crate::config::HomeZoneConfig::default(),
            history: crate::config::HistoryConfig::default(),
            mdns: Default::default(),
            zigbee: None,
        }
    }

    #[tokio::test]
    async fn initialized_state_uses_unprovisioned_mode_when_not_onboarded() {
        let storage = Storage::new_in_memory();
        storage
            .save_onboarding(&OnboardingState::default())
            .await
            .expect("save onboarding");

        let state = AppState::new_initialized(test_config(), storage)
            .await
            .expect("init app state");

        assert_eq!(state.core.runtime_mode(), RuntimeMode::UnprovisionedWifi);
    }

    #[tokio::test]
    async fn initialized_state_uses_operational_mode_when_onboarded() {
        let storage = Storage::new_in_memory();
        storage
            .save_onboarding(&OnboardingState {
                onboarded: true,
                done: vec!["user".into(), "core_config".into()],
                ..OnboardingState::default()
            })
            .await
            .expect("save onboarding");

        let state = AppState::new_initialized(test_config(), storage)
            .await
            .expect("init app state");

        assert_eq!(state.core.runtime_mode(), RuntimeMode::WifiOperational);
    }
}
