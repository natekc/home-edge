# Home Edge

Home Edge is an embedded, low-power Linux runtime for local-first automation deployments.

This repository contains the Milestone 0 foundation for a standalone runtime targeting low-power embedded Linux systems, benchmarked explicitly against Raspberry Pi Zero W class constraints.

## Goals

- Build reproducibly in Docker
- Cross-compile for `arm-unknown-linux-gnueabihf`
- Boot as a direct host binary on Raspberry Pi OS 32-bit
- Serve a minimal HTTP shell and health endpoint
- Persist onboarding state using atomic JSON writes with directory fsync
- Keep the runtime small and single-threaded by default

## Layout

- `crates/controller`: milestone 0 Home Edge runtime shell
- `.cargo/config.toml`: target linker configuration
- `config/default.toml`: default runtime config
- `docker/`: cross-build and smoke-test assets
- `systemd/`: native service unit
- `scripts/`: installation helpers

## Local development

```bash
cargo test
cargo run -- --config config/default.toml
```

## Docker build

```bash
docker build -f docker/Dockerfile.build --target runtime -t home-edge-build .
docker create --name extract home-edge-build
docker cp extract:/out/home-edge ./target/home-edge
```

## Current endpoints

- `GET /api/health`
- `GET /api/onboarding`
- `POST /api/onboarding/complete`
- `GET /`

## Milestone 0 limits

This is intentionally not a full Home Assistant replacement. It is the first native bring-up slice for build, boot, health, storage, and a minimal onboarding-state path.
