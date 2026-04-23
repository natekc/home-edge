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

## Installing on a device (no Rust required)

Download the pre-built tarball for your board from the [releases page](../../releases),
then from your laptop/desktop (on the same network as the device):

```bash
# First-time install
make push TARBALL=~/Downloads/home-edge-arm-unknown-linux-gnueabihf.tar.gz HOST=pi@raspberrypi.local

# Upgrade
make push TARBALL=~/Downloads/home-edge-arm-unknown-linux-gnueabihf.tar.gz HOST=pi@raspberrypi.local

# Rollback to the previous binary
make rollback HOST=pi@raspberrypi.local
```

Or without `make`, entirely by hand:

```bash
scp home-edge-arm-unknown-linux-gnueabihf.tar.gz pi@raspberrypi.local:/tmp/
ssh pi@raspberrypi.local '
  mkdir -p /tmp/home-edge-update &&
  tar -xzf /tmp/home-edge-update.tar.gz -C /tmp/home-edge-update &&
  sudo sh /tmp/home-edge-update/upgrade.sh
'
```

The device does not need internet access — the tarball is transferred over your local
network via SSH/SCP.  `upgrade.sh` delegates to `install.sh` automatically on first run.

### Release tarballs

| Tarball | Board | Notes |
|---|---|---|
| `home-edge-arm-unknown-linux-gnueabihf.tar.gz` | Raspberry Pi Zero W, Pi 1 | ARMv6, hard-float |
| `home-edge-armv7-unknown-linux-gnueabihf.tar.gz` | Raspberry Pi 2/3/4 (32-bit OS) | ARMv7 |
| `home-edge-aarch64-unknown-linux-gnu.tar.gz` | Raspberry Pi 3/4/5 (64-bit OS), most SBCs | AArch64 |
| `home-edge-riscv64gc-unknown-linux-gnu.tar.gz` | StarFive VisionFive 2, Milk-V Pioneer | RISC-V 64 |

## Local development

```bash
cargo test
cargo run -- --config config/default.toml
```

## Building from source

### Cross-compilation

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

On **Linux** (Debian/Ubuntu):

```bash
sudo apt install gcc-arm-linux-gnueabihf        # ARMv6 / ARMv7
sudo apt install gcc-aarch64-linux-gnu          # AArch64
```

See `.cargo/config.toml` for the full list of supported targets and required linker names.

### Build and deploy from source

```bash
# Build, package, and push to device in one command:
make deploy HOST=pi@raspberrypi.local

# Different board:
make deploy TARGET=aarch64-unknown-linux-gnu HOST=ubuntu@myboard.local

# Just build the tarball (for uploading to a release):
make package TARGET=arm-unknown-linux-gnueabihf
# → home-edge-arm-unknown-linux-gnueabihf.tar.gz
```

## Current endpoints

- `GET /api/health`
- `GET /api/onboarding`
- `POST /api/onboarding/complete`
- `GET /`

## Milestone 0 limits

This is intentionally not a full Home Assistant replacement. It is the first native bring-up slice for build, boot, health, storage, and a minimal onboarding-state path.
