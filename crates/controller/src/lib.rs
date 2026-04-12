#[cfg(all(feature = "transport_wifi", feature = "transport_ble"))]
compile_error!("home-edge supports exactly one transport feature at a time");

#[cfg(not(any(feature = "transport_wifi", feature = "transport_ble")))]
compile_error!("home-edge requires exactly one transport feature");

pub mod app;
pub mod config;
pub mod core;
pub mod history_store;
pub mod logging;
pub mod notification_store;
pub mod service;
pub mod state_store;
pub mod storage;
pub mod templates;

#[cfg(feature = "transport_wifi")]
pub mod area_registry_store;
#[cfg(feature = "transport_wifi")]
pub mod auth_store;
#[cfg(feature = "transport_wifi")]
pub mod ha_api;
#[cfg(feature = "transport_wifi")]
pub mod ha_auth;
#[cfg(feature = "transport_wifi")]
pub mod ha_mobile;
#[cfg(feature = "transport_wifi")]
pub mod ha_webhook;
#[cfg(feature = "transport_wifi")]
pub mod ha_ws;
#[cfg(feature = "transport_wifi")]
pub mod http;
#[cfg(feature = "transport_wifi")]
pub mod mobile_device_store;
#[cfg(feature = "transport_wifi")]
pub mod mobile_entity_store;
#[cfg(feature = "transport_wifi")]
pub mod zeroconf;
