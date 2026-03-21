# Rust MVP Milestone 0

This directory contains the Milestone 0 foundation for a low-power Linux control-plane runtime targeting Raspberry Pi Zero W class devices first.

## Goals

- Build reproducibly in Docker
- Cross-compile for `arm-unknown-linux-gnueabihf`
- Boot as a direct host binary on Raspberry Pi OS 32-bit
- Serve a minimal HTTP shell and health endpoint
- Persist onboarding state using atomic JSON writes with directory fsync
- Keep the runtime small and single-threaded by default

## Layout

- `crates/controller`: milestone 0 runtime shell
- `.cargo/config.toml`: target linker configuration
- `config/default.toml`: default runtime config
- `docker/`: cross-build and smoke-test assets
- `systemd/`: native service unit
- `scripts/`: installation helpers

## Local development

```bash
cd rust-mvp
cargo test
cargo run -- --config config/default.toml
```

## Docker build

```bash
cd rust-mvp
docker build -f docker/Dockerfile.build --target runtime -t pi-control-plane-build .
docker create --name extract pi-control-plane-build
docker cp extract:/out/pi-control-plane ./target/pi-control-plane
```

## Current endpoints

- `GET /api/health`
- `GET /api/onboarding`
- `POST /api/onboarding/complete`
- `GET /`

## Milestone 0 limits

This is intentionally not a full Home Assistant replacement. It is the first native bring-up slice for build, boot, health, storage, and a minimal onboarding-state path.
