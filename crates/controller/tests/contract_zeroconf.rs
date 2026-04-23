#![cfg(feature = "transport_wifi")]

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use home_edge::config::{AppConfig, ServerConfig, StorageConfig, UiConfig};
use home_edge::storage::{OnboardingState, StoredUser};
use home_edge::zeroconf::{ZEROCONF_TYPE, build_service_info};

fn sample_config() -> AppConfig {
    AppConfig {
        server: ServerConfig {
            host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 8124,
            log_level: tracing::Level::INFO,
        },
        storage: StorageConfig {
            data_dir: PathBuf::from("/tmp/home-edge-test"),
        },
        ui: UiConfig {
            product_name: "Home Edge".into(),
        },
        areas: home_edge::config::AreasConfig::default(),
        home_zone: home_edge::config::HomeZoneConfig::default(),
        history: home_edge::config::HistoryConfig::default(),
            mdns: Default::default(),
    }
}

fn onboarded_state() -> OnboardingState {
    OnboardingState {
        onboarded: true,
        done: vec!["user".into(), "core_config".into()],
        user: Some(StoredUser {
            name: "Test User".into(),
            username: "test-user".into(),
            password: "test-pass".into(),
            language: "en".into(),
        }),
        location_name: Some("Living Room".into()),
        country: Some("US".into()),
        language: Some("en".into()),
        time_zone: Some("UTC".into()),
        unit_system: Some("metric".into()),
        ..OnboardingState::default()
    }
}

#[test]
fn zeroconf_contract_matches_home_assistant_service_type_and_fields() {
    let service = build_service_info(
        &sample_config(),
        &onboarded_state(),
        "123e4567-e89b-12d3-a456-426614174000",
        &[IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42))],
    )
    .expect("service info");

    assert_eq!(service.get_type(), ZEROCONF_TYPE);
    assert_eq!(
        service.get_fullname(),
        "Living Room._home-assistant._tcp.local."
    );
    // hostname is the system hostname, not the instance UUID
    let expected_host = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| {
            let h = h.trim_end_matches('.');
            let h = h.strip_suffix(".local").unwrap_or(h);
            format!("{h}.local.")
        })
        .unwrap_or_else(|| "123e4567-e89b-12d3-a456-426614174000.local.".into());
    assert_eq!(service.get_hostname(), expected_host);
    assert_eq!(service.get_port(), 8124);
    assert_eq!(
        service.get_property_val_str("location_name"),
        Some("Living Room")
    );
    assert_eq!(
        service.get_property_val_str("uuid"),
        Some("123e4567-e89b-12d3-a456-426614174000")
    );
    assert_eq!(
        service.get_property_val_str("version"),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        service.get_property_val_str("internal_url"),
        Some("http://192.168.1.42:8124")
    );
    assert_eq!(
        service.get_property_val_str("base_url"),
        Some("http://192.168.1.42:8124")
    );
    assert_eq!(
        service.get_property_val_str("requires_api_password"),
        Some("true")
    );
}

#[test]
fn zeroconf_contract_uses_product_name_before_onboarding_location_exists() {
    let service = build_service_info(
        &sample_config(),
        &OnboardingState::default(),
        "instance-1",
        &[IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))],
    )
    .expect("service info");

    assert_eq!(
        service.get_property_val_str("location_name"),
        Some("Home Edge")
    );
    assert_eq!(
        service.get_fullname(),
        "Home Edge._home-assistant._tcp.local."
    );
}
