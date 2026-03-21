# Milestone 0

## Scope

Milestone 0 is the foundation spike for a native low-power Linux control-plane runtime.

Included:
- reproducible Rust workspace
- native boot path on Raspberry Pi Zero W class Linux
- Docker cross-build path for `arm-unknown-linux-gnueabihf`
- minimal HTTP shell and health endpoint
- atomic JSON persistence shell with directory fsync
- systemd packaging assets
- written onboarding, UI, and storage contracts

Excluded:
- full onboarding implementation
- MQTT runtime behavior
- GPIO runtime behavior
- automations
- discovery
- rich dashboard UI

## Exit criteria

- `cargo test` passes
- Docker build produces an armhf artifact
- service can be installed as a native host binary
- runtime creates and loads its data directory
- onboarding state persists durably enough for basic reboot and interruption testing
