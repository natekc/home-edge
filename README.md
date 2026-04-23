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
# First-time install or upgrade
cargo xtask push --tarball ~/Downloads/home-edge-arm-unknown-linux-gnueabihf.tar.gz --host pi@192.168.1.50

# Roll back to the previous binary
cargo xtask rollback --host pi@192.168.1.50
```

`--host` is required and must be a `user@address` SSH destination.
`cargo xtask` is just a Cargo alias — no extra tools required beyond `cargo`, `ssh`, and `scp`.

Timeout defaults are 10 s connect + 30 s stall tolerance (5 s × 6 probes).
Override with `--connect-timeout`, `--alive-interval`, `--alive-count`.

If you don't have Rust at all, the same steps work directly:

```bash
scp home-edge-arm-unknown-linux-gnueabihf.tar.gz pi@192.168.1.50:/var/tmp/
ssh pi@192.168.1.50 '
  mkdir -p /var/tmp/home-edge-update &&
  tar -xzf /var/tmp/home-edge-update.tar.gz -C /var/tmp/home-edge-update &&
  sudo sh /var/tmp/home-edge-update/upgrade.sh
'
```

The device does not need internet access — the tarball is transferred over your local
network via SSH/SCP. `upgrade.sh` delegates to `install.sh` automatically on first run.

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
cargo xtask deploy --host pi@192.168.1.50
cargo xtask deploy --target aarch64-unknown-linux-gnu --host ubuntu@myboard.local

# Just build the tarball (e.g. for uploading to a release):
cargo xtask package
cargo xtask package --target aarch64-unknown-linux-gnu
# → home-edge-<target>.tar.gz
```

See `cargo xtask --help` and `cargo xtask <command> --help` for all options.

## Current endpoints

- `GET /api/health`
- `GET /api/onboarding`
- `POST /api/onboarding/complete`
- `GET /`

## Milestone 0 limits

This is intentionally not a full Home Assistant replacement. It is the first native bring-up slice for build, boot, health, storage, and a minimal onboarding-state path.
