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

## Cross-compilation

Install Rust targets once:

```bash
rustup target add arm-unknown-linux-gnueabihf   # Pi Zero W / Pi 1
rustup target add armv7-unknown-linux-gnueabihf  # Pi 2/3/4 32-bit
rustup target add aarch64-unknown-linux-gnu      # Pi 3/4/5 64-bit
rustup target add riscv64gc-unknown-linux-gnu    # RISC-V 64-bit
```

On **macOS**, install the cross-linker toolchain via the
[messense tap](https://github.com/messense/homebrew-macos-cross-toolchains):

```bash
brew tap messense/macos-cross-toolchains
# install the one you need, e.g.:
brew install messense/macos-cross-toolchains/arm-unknown-linux-gnueabihf
```

On **Linux** (Debian/Ubuntu), install the distro cross package:

```bash
sudo apt install gcc-arm-linux-gnueabihf        # ARMv6
sudo apt install gcc-arm-linux-gnueabihf        # ARMv7 (same package)
sudo apt install gcc-aarch64-linux-gnu          # AArch64
```

Build:

```bash
cargo build --release --target arm-unknown-linux-gnueabihf -p home-edge
# binary: target/arm-unknown-linux-gnueabihf/release/home-edge
```

See `.cargo/config.toml` for the full list of supported targets and required linker names.

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
