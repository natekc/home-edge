use std::collections::HashMap;
use std::net::IpAddr;

use anyhow::{Context, Result, anyhow};
use local_ip_address::list_afinet_netifas;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tracing::warn;

use crate::app::AppState;
use crate::config::AppConfig;
use crate::storage::OnboardingState;

pub const ZEROCONF_TYPE: &str = "_home-assistant._tcp.local.";

pub struct ZeroconfRegistration {
    daemon: ServiceDaemon,
}

impl Drop for ZeroconfRegistration {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

pub async fn announce(state: &AppState) -> Result<Option<ZeroconfRegistration>> {
    let addresses = discover_announce_addresses()?;
    if addresses.is_empty() {
        warn!("no routable network addresses found for zeroconf advertisement");
        return Ok(None);
    }

    let (onboarding, instance_id) = tokio::try_join!(
        async { state.storage.load_onboarding().await.context("load onboarding for zeroconf") },
        async { state.storage.load_or_create_instance_id().await.context("load instance id for zeroconf") },
    )?;

    let service = build_service_info(&state.config, &onboarding, &instance_id, &addresses)?;
    let daemon = ServiceDaemon::new().context("create zeroconf daemon")?;
    daemon
        .register(service)
        .context("register zeroconf service")?;

    Ok(Some(ZeroconfRegistration { daemon }))
}

pub fn build_service_info(
    config: &AppConfig,
    onboarding: &OnboardingState,
    instance_id: &str,
    addresses: &[IpAddr],
) -> Result<ServiceInfo> {
    if addresses.is_empty() {
        return Err(anyhow!("at least one announce address is required"));
    }

    let location_name = truncate_instance_name(
        onboarding
            .location_name
            .as_deref()
            .unwrap_or(&config.ui.product_name),
    );
    let host_name = format!("{instance_id}.local.");
    let properties = build_properties(config, onboarding, instance_id, addresses);

    ServiceInfo::new(
        ZEROCONF_TYPE,
        &location_name,
        &host_name,
        addresses,
        config.server.port,
        properties,
    )
    .context("build zeroconf service info")
}

pub fn discover_announce_addresses() -> Result<Vec<IpAddr>> {
    let mut addresses = Vec::new();
    for (_name, address) in list_afinet_netifas().context("list network interfaces")? {
        if address.is_loopback() || address.is_unspecified() {
            continue;
        }
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    Ok(addresses)
}

fn build_properties(
    config: &AppConfig,
    onboarding: &OnboardingState,
    instance_id: &str,
    addresses: &[IpAddr],
) -> HashMap<String, String> {
    let location_name = onboarding
        .location_name
        .clone()
        .unwrap_or_else(|| config.ui.product_name.clone());

    let base_url = addresses
        .iter()
        .find(|address| address.is_ipv4())
        .or_else(|| addresses.first())
        .map(|address| format!("http://{address}:{}", config.server.port))
        .unwrap_or_default();

    HashMap::from([
        ("location_name".into(), location_name),
        ("uuid".into(), instance_id.to_string()),
        ("version".into(), env!("CARGO_PKG_VERSION").into()),
        ("external_url".into(), String::new()),
        ("internal_url".into(), base_url.clone()),
        ("base_url".into(), base_url),
        ("requires_api_password".into(), "true".into()),
    ])
}

fn truncate_instance_name(value: &str) -> String {
    const MAX_BYTES: usize = 63;
    if value.len() <= MAX_BYTES {
        return value.to_string();
    }

    let mut truncated = String::new();
    for character in value.chars() {
        let next_len = truncated.len() + character.len_utf8();
        if next_len > MAX_BYTES {
            break;
        }
        truncated.push(character);
    }
    truncated
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;

    use super::*;
    use crate::config::{AppConfig, ServerConfig, StorageConfig, UiConfig};
    use crate::storage::{OnboardingState, StoredUser};

    fn sample_config() -> AppConfig {
        AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: 8124,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/home-edge-test"),
            },
            ui: UiConfig {
                product_name: "Home Edge".into(),
            },
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
            location_name: Some("My Home".into()),
            country: Some("US".into()),
            language: Some("en".into()),
            time_zone: Some("UTC".into()),
            unit_system: Some("metric".into()),
            ..OnboardingState::default()
        }
    }

    #[test]
    fn builds_home_assistant_service_info() {
        let service = build_service_info(
            &sample_config(),
            &onboarded_state(),
            "123e4567-e89b-12d3-a456-426614174000",
            &[IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))],
        )
        .expect("service info");

        assert_eq!(service.get_type(), ZEROCONF_TYPE);
        assert_eq!(
            service.get_fullname(),
            "My Home._home-assistant._tcp.local."
        );
        assert_eq!(
            service.get_hostname(),
            "123e4567-e89b-12d3-a456-426614174000.local."
        );
        assert_eq!(service.get_port(), 8124);
        assert_eq!(
            service.get_property_val_str("location_name"),
            Some("My Home")
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
            Some("http://192.168.1.20:8124")
        );
        assert_eq!(
            service.get_property_val_str("base_url"),
            Some("http://192.168.1.20:8124")
        );
        assert_eq!(
            service.get_property_val_str("requires_api_password"),
            Some("true")
        );
    }

    #[test]
    fn falls_back_to_product_name_when_location_name_missing() {
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
}
