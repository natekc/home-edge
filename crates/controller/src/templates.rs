//! Build a `minijinja::Environment` with all UI templates embedded at
//! compile time via `include_str!()`.

use minijinja::{AutoEscape, Environment};

pub fn build_env() -> Environment<'static> {
    let mut env = Environment::new();
    // Auto-escape HTML in all .html templates to prevent XSS.
    env.set_auto_escape_callback(|name: &str| {
        if name.ends_with(".html") {
            AutoEscape::Html
        } else {
            AutoEscape::None
        }
    });

    // Design system fragments
    env.add_template("_css.html", include_str!("../templates/_css.html"))
        .expect("_css.html");
    env.add_template("_icons.html", include_str!("../templates/_icons.html"))
        .expect("_icons.html");
    // htmx inlined: no CDN dependency, auto-escape disabled for .js files (see callback above)
    env.add_template("_htmx.js", include_str!("../templates/_htmx.js"))
        .expect("_htmx.js");

    // Base shells
    env.add_template(
        "_base_wizard.html",
        include_str!("../templates/_base_wizard.html"),
    )
    .expect("_base_wizard.html");
    env.add_template(
        "_base_app.html",
        include_str!("../templates/_base_app.html"),
    )
    .expect("_base_app.html");

    // Full pages
    env.add_template("authorize.html", include_str!("../templates/authorize.html"))
        .expect("authorize.html");
    env.add_template(
        "onboarding.html",
        include_str!("../templates/onboarding.html"),
    )
    .expect("onboarding.html");
    env.add_template("dashboard.html", include_str!("../templates/dashboard.html"))
        .expect("dashboard.html");
    env.add_template(
        "device_detail.html",
        include_str!("../templates/device_detail.html"),
    )
    .expect("device_detail.html");
    env.add_template(
        "entity_edit.html",
        include_str!("../templates/entity_edit.html"),
    )
    .expect("entity_edit.html");
    env.add_template("ble_scan.html", include_str!("../templates/ble_scan.html"))
        .expect("ble_scan.html");
    env.add_template("settings.html", include_str!("../templates/settings.html"))
        .expect("settings.html");
    env.add_template("profile.html", include_str!("../templates/profile.html"))
        .expect("profile.html");
    env.add_template("devices.html", include_str!("../templates/devices.html"))
        .expect("devices.html");
    env.add_template("history.html", include_str!("../templates/history.html"))
        .expect("history.html");
    env.add_template("logbook.html", include_str!("../templates/logbook.html"))
        .expect("logbook.html");
    env.add_template("developer_tools.html", include_str!("../templates/developer_tools.html"))
        .expect("developer_tools.html");
    env.add_template("notifications.html", include_str!("../templates/notifications.html"))
        .expect("notifications.html");
    env.add_template("system.html", include_str!("../templates/system.html"))
        .expect("system.html");
    env.add_template("areas.html", include_str!("../templates/areas.html"))
        .expect("areas.html");
    env.add_template("area_detail.html", include_str!("../templates/area_detail.html"))
        .expect("area_detail.html");

    // HTMX fragment partials
    env.add_template(
        "fragments/sensors.html",
        include_str!("../templates/fragments/sensors.html"),
    )
    .expect("fragments/sensors.html");
    env.add_template(
        "fragments/ble_results.html",
        include_str!("../templates/fragments/ble_results.html"),
    )
    .expect("fragments/ble_results.html");

    // More-info domain dialogs
    env.add_template("more_info/_light.html",         include_str!("../templates/more_info/_light.html")).expect("more_info/_light.html");
    env.add_template("more_info/_switch.html",        include_str!("../templates/more_info/_switch.html")).expect("more_info/_switch.html");
    env.add_template("more_info/_cover.html",         include_str!("../templates/more_info/_cover.html")).expect("more_info/_cover.html");
    env.add_template("more_info/_lock.html",          include_str!("../templates/more_info/_lock.html")).expect("more_info/_lock.html");
    env.add_template("more_info/_fan.html",           include_str!("../templates/more_info/_fan.html")).expect("more_info/_fan.html");
    env.add_template("more_info/_sensor.html",        include_str!("../templates/more_info/_sensor.html")).expect("more_info/_sensor.html");
    env.add_template("more_info/_binary_sensor.html", include_str!("../templates/more_info/_binary_sensor.html")).expect("more_info/_binary_sensor.html");
    env.add_template("more_info/_button.html",        include_str!("../templates/more_info/_button.html")).expect("more_info/_button.html");
    env.add_template("more_info/_scene.html",         include_str!("../templates/more_info/_scene.html")).expect("more_info/_scene.html");
    env.add_template("more_info/_script.html",        include_str!("../templates/more_info/_script.html")).expect("more_info/_script.html");
    env.add_template("more_info/_select.html",        include_str!("../templates/more_info/_select.html")).expect("more_info/_select.html");
    env.add_template("more_info/_climate.html",       include_str!("../templates/more_info/_climate.html")).expect("more_info/_climate.html");
    env.add_template("more_info/_default.html",       include_str!("../templates/more_info/_default.html")).expect("more_info/_default.html");

    env
}

/// Verify that every template file on disk is registered in the minijinja Environment.
///
/// This test prevents the recurring bug where a template is added to templates/ but
/// forgotten in this file — causing HTTP 500 at render time (no HTML → no
/// connection-status:connected → iOS 10s disconnect timer fires → black screen).
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn collect_template_files(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read templates dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                collect_template_files(base, &path, out);
            } else {
                let rel = path.strip_prefix(base).expect("strip prefix");
                out.push(rel.to_str().expect("utf-8 path").replace('\\', "/"));
            }
        }
    }

    #[test]
    fn every_template_file_is_registered() {
        let env = build_env();
        let templates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let mut files = Vec::new();
        collect_template_files(&templates_dir, &templates_dir, &mut files);
        files.sort();

        let mut missing = Vec::new();
        for name in &files {
            if env.get_template(name).is_err() {
                missing.push(name.clone());
            }
        }

        assert!(
            missing.is_empty(),
            "template files exist on disk but are NOT registered in templates.rs:\n  {}",
            missing.join("\n  ")
        );
    }
}
