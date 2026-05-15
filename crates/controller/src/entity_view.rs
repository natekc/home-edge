//! Backend-agnostic entity view-model for templates.
//!
//! [`EntityView`] is the single serializable struct passed to all Minijinja
//! templates that render an entity: the dashboard, more-info panels, Zigbee
//! device detail pages, and any future backend-specific detail pages.
//!
//! Keeping this type in its own module enforces that no single backend owns
//! the presentation contract.  Each backend produces an `EntityView` from its
//! own record type; the HTTP handlers and templates consume it uniformly.
//!
//! # Extension point
//!
//! To add a new entity backend:
//! 1. Create the backend's store/record module (e.g. `wifi_sensor_store.rs`).
//! 2. Implement `pub(crate) fn entity_view_for(record, states) -> EntityView`
//!    in that module (see `zigbee_integration::entity_view_for` as a reference).
//! 3. Add one `#[cfg(feature = "…")]` arm in `crate::http::fetch_entity_view`.
//!
//! Nothing in *this* module needs to change between backends.

use serde::Serialize;

/// A serializable view of a single HA entity consumed by Minijinja templates.
///
/// All fields are `pub(crate)` so every backend module can construct the
/// struct directly without indirection.
#[derive(Serialize, Clone)]
pub(crate) struct EntityView {
    pub(crate) entity_id: String,
    /// Webhook registration ID for mobile-app entities.
    /// `None` for Zigbee entities and any other non-mobile backend.
    /// Templates that navigate to mobile-device pages must check for `Some`.
    pub(crate) webhook_id: Option<String>,
    pub(crate) display_name: String,
    /// HA platform domain: `light`, `switch`, `sensor`, `binary_sensor`, …
    pub(crate) entity_type: String,
    /// Material Design icon name (without `mdi:` prefix) for the entity list.
    pub(crate) icon_name: String,
    /// Human-readable state string (e.g. `"on"`, `"21.5"`, `"unavailable"`).
    pub(crate) value: String,
    /// Formatted unit string (e.g. `"°C"`, `"%"`, `""` when absent).
    pub(crate) unit: String,
    pub(crate) device_class: String,
    pub(crate) user_area_id: String,
    pub(crate) unit_of_measurement: Option<String>,
    pub(crate) disabled: bool,
    /// HA service action for controllable entities (e.g. `"toggle"`, `"press"`);
    /// empty string for read-only entities (sensor, binary_sensor).
    pub(crate) service_action: String,
    pub(crate) current_temperature: Option<f64>,
    pub(crate) target_temperature: Option<f64>,
    pub(crate) hvac_modes: Vec<String>,
    /// Light brightness 0–255, `None` if unavailable.
    /// Source: homeassistant/components/light/__init__.py ATTR_BRIGHTNESS
    pub(crate) brightness: Option<u8>,
    /// Light color temperature in kelvin, `None` if unavailable.
    /// Source: homeassistant/components/light/__init__.py ATTR_COLOR_TEMP_KELVIN
    pub(crate) color_temp_kelvin: Option<u16>,
    /// Per-device minimum color temperature in kelvin.
    /// Source: homeassistant/components/light/__init__.py ATTR_MIN_COLOR_TEMP_KELVIN, DEFAULT_MIN_KELVIN = 2000
    pub(crate) min_color_temp_kelvin: u16,
    /// Per-device maximum color temperature in kelvin.
    /// Source: homeassistant/components/light/__init__.py ATTR_MAX_COLOR_TEMP_KELVIN, DEFAULT_MAX_KELVIN = 6535
    pub(crate) max_color_temp_kelvin: u16,
    /// Active color mode for lights ("hs", "rgb", "color_temp", "brightness", "onoff").
    /// Source: homeassistant/components/light/__init__.py ATTR_COLOR_MODE, ColorMode
    pub(crate) color_mode: Option<String>,
    /// HS color: hue 0–360, saturation 0–100.
    /// Source: homeassistant/components/light/__init__.py ATTR_HS_COLOR
    pub(crate) hs_color: Option<(f32, f32)>,
    /// RGB color (r, g, b each 0–255).
    /// Source: homeassistant/components/light/__init__.py ATTR_RGB_COLOR
    pub(crate) rgb_color: Option<(u8, u8, u8)>,
    /// Pre-formatted "#rrggbb" hex string derived from rgb_color; used directly in templates.
    pub(crate) rgb_hex: Option<String>,
    /// Supported color modes list.
    /// Source: homeassistant/components/light/__init__.py ATTR_SUPPORTED_COLOR_MODES
    pub(crate) supported_color_modes: Vec<String>,
    /// Select entity available options.
    pub(crate) options: Vec<String>,
    /// Cover current position 0–100, `None` if unavailable.
    pub(crate) current_position: Option<u8>,
    /// Fan speed percentage 0–100, `None` if unavailable.
    /// Source: homeassistant/components/fan/__init__.py ATTR_PERCENTAGE
    pub(crate) fan_percentage: Option<u8>,
    /// For Zigbee entities: the display name of the parent device (e.g. `"SNZB-02"`).
    /// Used to group unassigned entities by device on the dashboard overview.
    /// `None` for mobile-app entities.
    pub(crate) device_name: Option<String>,
    /// True when the entity state is `"unavailable"` or `"unknown"`.
    /// Source: homeassistant/const.py STATE_UNAVAILABLE, STATE_UNKNOWN
    pub(crate) is_unavailable: bool,
}

/// Area-grouped card passed to the dashboard template.
#[derive(Serialize)]
pub(crate) struct AreaCard {
    pub(crate) area_name: String,
    pub(crate) entities: Vec<EntityView>,
    /// Area temperature display string (e.g. "20.1 °C"), populated when a
    /// temperature sensor entity exists in the area.
    /// Source: homeassistant/helpers/area_registry.py AreaEntry.temperature_entity_id
    pub(crate) area_temp: Option<String>,
    /// Area humidity display string (e.g. "52 %"), populated when a
    /// humidity sensor entity exists in the area.
    /// Source: homeassistant/helpers/area_registry.py AreaEntry.humidity_entity_id
    pub(crate) area_humidity: Option<String>,
}
