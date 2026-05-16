//! UX parity tests — enforces that home-edge templates maintain fidelity with the
//! Home Assistant core design system.
//!
//! Source of truth: docs/ux-parity.toml
//!
//! Run:
//!   cargo test ux_parity
//!   cargo test --features zigbee ux_parity   (also tests Zigbee-only pages)
//!
//! Four deterministic checks:
//!   1. css_tokens_all_defined            — every required CSS token is in _css.html
//!   2. all_pages_render_without_error    — every page renders with fixture data
//!   3. pages_have_required_classes       — each page contains its required CSS classes
//!   4. nav_items_present_and_ordered     — sidebar nav matches the manifest order

#![cfg(feature = "transport_wifi")]

use serde::Deserialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Manifest types  (mirrors docs/ux-parity.toml)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Manifest {
    css_tokens: CssTokens,
    nav: Vec<NavItem>,
    pages: Vec<Page>,
}

#[derive(Deserialize)]
struct CssTokens {
    required: Vec<String>,
}

#[derive(Deserialize)]
struct NavItem {
    id: String,
    href: String,
    #[serde(default)]
    extension: bool,
}

#[derive(Deserialize)]
struct Page {
    template: String,
    required_classes: Vec<String>,
    #[serde(default)]
    zigbee_only: bool,
}

// Paths are relative to this source file (crates/controller/tests/ux_parity.rs).
const MANIFEST_SRC: &str = include_str!("../../../docs/ux-parity.toml");
const CSS_SRC: &str = include_str!("../templates/_css.html");

fn manifest() -> Manifest {
    toml::from_str(MANIFEST_SRC).expect("docs/ux-parity.toml must parse without errors")
}

// ---------------------------------------------------------------------------
// Fixture contexts — minimum data needed to render each template
// ---------------------------------------------------------------------------

/// Returns a base JSON context satisfying every variable in _base_app.html.
fn base_ctx(active_page: &str, extra: Value) -> Value {
    let mut ctx = json!({
        "product_name":          "Home Edge",
        "location_name":         "Test Home",
        "transport":             "WiFi",
        "is_ble_build":          false,
        "zigbee_enabled":        cfg!(feature = "zigbee"),
        "zigbee_configured":     false,
        "active_page":           active_page,
        "server_host":           "192.168.1.100",
        "server_port":           8124,
        "areas":                 [],
    });
    if let (Some(obj), Some(extras)) = (ctx.as_object_mut(), extra.as_object()) {
        for (k, v) in extras {
            obj.insert(k.clone(), v.clone());
        }
    }
    ctx
}

/// Returns a fixture context for every page listed in the manifest.
fn fixture_ctx(template: &str) -> Value {
    match template {
        "dashboard.html" => base_ctx("dashboard", json!({
            "devices":        [],
            "zigbee_devices": [],
            "has_any_device": false,
            "area_cards":     [],
        })),
        "history.html" => base_ctx("history", json!({
            "entities": [],
        })),
        "logbook.html" => base_ctx("logbook", json!({
            "entries": [],
        })),
        "developer_tools.html" => base_ctx("developer-tools", json!({
            "entity_states": [],
        })),
        "settings.html" => base_ctx("settings", json!({
            "version":      "0.1.0",
            "runtime_mode": "Active",
        })),
        "areas.html" => base_ctx("areas", json!({
            "zones":      [],
            "all_labels": [],
            "back_url":   "/settings",
            "nav_title":  "Areas, labels & zones",
        })),
        "devices.html" => base_ctx("settings", json!({
            "devices": [],
        })),
        "notifications.html" => base_ctx("notifications", json!({
            "notifications": [],
        })),
        "profile.html" => base_ctx("profile", json!({
            "user_name":     "Test User",
            "user_username": "test-user",
            "language":      "en",
            "time_zone":     "UTC",
            "unit_system":   "metric",
            "country":       "US",
            "access_tokens": [],
        })),
        "system.html" => base_ctx("system", json!({
            "version":      "0.1.0",
            "runtime_mode": "Active",
        })),
        "zigbee_devices.html" => base_ctx("zigbee", json!({
            "devices":               [],
            "bridge_running":        false,
            "bridge_error":          null,
            "serial_port":           "",
            "pairing_remaining_secs": 0,
        })),
        _ => base_ctx("", json!({})),
    }
}

// ---------------------------------------------------------------------------
// CSS class token matcher
//
// Returns true when `class` appears as a complete word inside a class=""
// attribute in the rendered HTML.  Handles:
//   class="ha-card"                 (sole class)
//   class="ha-card some-other"      (first class)
//   class="some-other ha-card"      (last class)
//   class="a ha-card b"             (middle class)
//   class="transport-badge "        (trailing space from template expression)
// ---------------------------------------------------------------------------
fn html_has_class(html: &str, class: &str) -> bool {
    let bytes = html.as_bytes();
    let class_bytes = class.as_bytes();
    let len = class_bytes.len();
    let mut start = 0;
    while let Some(offset) = html[start..].find(class) {
        let pos = start + offset;
        // Character before the class name must be `"` or ` `
        let before_ok = pos == 0 || matches!(bytes[pos - 1], b'"' | b' ');
        // Character after the class name must be `"`, ` `, or end-of-string
        let after = bytes.get(pos + len).copied();
        let after_ok = matches!(after, None | Some(b'"') | Some(b' '));
        if before_ok && after_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

// ---------------------------------------------------------------------------
// Test 1: every required CSS token is declared in _css.html
// ---------------------------------------------------------------------------

#[test]
fn css_tokens_all_defined() {
    let m = manifest();
    let missing: Vec<_> = m
        .css_tokens
        .required
        .iter()
        .filter(|token| {
            // Each token must appear as a CSS custom-property declaration, e.g. "--primary-color:"
            let needle = format!("{}:", token);
            !CSS_SRC.contains(needle.as_str())
        })
        .collect();

    assert!(
        missing.is_empty(),
        "CSS tokens in docs/ux-parity.toml [css_tokens.required] are missing \
         from _css.html:\n  {}",
        missing
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Test 2: every page renders without a template error
// ---------------------------------------------------------------------------

#[test]
fn all_pages_render_without_error() {
    let env = home_edge::templates::build_env();
    let mut failures: Vec<String> = Vec::new();

    for page in manifest().pages {
        if page.zigbee_only && !cfg!(feature = "zigbee") {
            continue;
        }
        let tmpl = match env.get_template(&page.template) {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("{}: template not registered — {e}", page.template));
                continue;
            }
        };
        if let Err(e) = tmpl.render(fixture_ctx(&page.template)) {
            failures.push(format!("{}: render error — {e}", page.template));
        }
    }

    assert!(
        failures.is_empty(),
        "Pages failed to render with fixture data:\n  {}",
        failures.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Test 3: each page contains its required structural CSS classes
// ---------------------------------------------------------------------------

#[test]
fn pages_have_required_structural_classes() {
    let env = home_edge::templates::build_env();
    let mut failures: Vec<String> = Vec::new();

    for page in manifest().pages {
        if page.zigbee_only && !cfg!(feature = "zigbee") {
            continue;
        }
        let tmpl = match env.get_template(&page.template) {
            Ok(t) => t,
            Err(_) => continue, // already reported in test 2
        };
        let html = match tmpl.render(fixture_ctx(&page.template)) {
            Ok(h) => h,
            Err(_) => continue, // already reported in test 2
        };
        for class in &page.required_classes {
            if !html_has_class(&html, class) {
                failures.push(format!("{}: missing class '{class}'", page.template));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "Pages are missing required CSS classes from docs/ux-parity.toml:\n  {}",
        failures.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Test 4: sidebar nav items are present and appear in the declared order
// ---------------------------------------------------------------------------

#[test]
fn nav_items_present_and_ordered() {
    let env = home_edge::templates::build_env();
    let m = manifest();

    // Use dashboard.html (guaranteed nav_page) as the probe surface.
    let html = env
        .get_template("dashboard.html")
        .expect("dashboard.html must be registered")
        .render(base_ctx("dashboard", json!({
            "devices":        [],
            "zigbee_devices": [],
            "has_any_device": false,
            "area_cards":     [],
        })))
        .expect("dashboard.html must render");

    // Filter out extension items when the matching feature is inactive.
    let active_items: Vec<_> = m
        .nav
        .iter()
        .filter(|n| !(n.extension && !cfg!(feature = "zigbee")))
        .collect();

    // Locate each item's first occurrence by its href in the rendered sidebar.
    let ordered_positions: Vec<(usize, &str)> = active_items
        .iter()
        .map(|item| {
            let needle = format!("href=\"{}\"", item.href);
            let pos = html.find(needle.as_str()).unwrap_or_else(|| {
                panic!(
                    "Nav item '{}' (href='{}') not found in rendered dashboard.html.\n\
                     Add it to _base_app.html or update docs/ux-parity.toml.",
                    item.id, item.href
                )
            });
            (pos, item.id.as_str())
        })
        .collect();

    // Assert items appear in manifest-declared order (monotonically increasing positions).
    for w in ordered_positions.windows(2) {
        let (prev_pos, prev_id) = w[0];
        let (next_pos, next_id) = w[1];
        assert!(
            prev_pos < next_pos,
            "Nav ordering violation: '{}' (pos {}) must appear before '{}' (pos {}) \
             in the sidebar. Check _base_app.html nav order against docs/ux-parity.toml.",
            prev_id, prev_pos, next_id, next_pos
        );
    }
}
