# Home Edge

Home Edge is an embedded, low-power Linux runtime for local-first home automation — a full Home Assistant–compatible edge controller that runs on a Raspberry Pi Zero W or any ARMv6/ARMv7/AArch64/RISC-V board.

It speaks the native Home Assistant REST and WebSocket protocols so the official iOS and Android companion apps connect to it out of the box, with no cloud dependency.

## Features

- **HA-compatible API** — REST (`/api/*`), WebSocket (`/api/websocket`), and mobile webhook endpoints match the Home Assistant wire protocol so the companion apps pair without modification
- **mDNS auto-discovery** — advertises `_home-assistant._tcp.local.` so the iOS app finds the device on the local network without entering an IP address
- **Zigbee integration** — optional embedded [zigbee2mqtt-rs](https://github.com/home-edge/zigbee2mqtt-rs) bridge; pairs coordinator dongles over USB and surfaces devices and sensor states through the standard entity model
- **Onboarding flow** — multi-step onboarding (user account, location/time zone, analytics, integration) mirrors the HA core onboarding flow
- **Area and zone registry** — create rooms, assign entities; define geographic geofence zones for device-tracker automations
- **History and logbook** — per-entity ring-buffer history and logbook, surfaced through the web UI and the edge history API
- **Server-rendered web UI** — dashboard, devices, areas, zones, history, logbook, developer tools, notifications, and system pages served via minijinja templates
- **Transport-selectable build** — compile with `transport_wifi` (default, full feature set) or `transport_ble` (BLE scaffold)
- **One-command deploy** — cross-compile and push to a device over SSH in a single `cargo xtask deploy` invocation

## Layout

- `crates/controller/` — Home Edge runtime (REST/WS API, web UI, Zigbee integration, stores)
- `crates/ha-types/` — shared HA wire-protocol types
- `xtask/` — build and deployment tasks (`cargo xtask`)
- `.cargo/config.toml` — target linker configuration
- `config/default.toml` — default runtime config
- `docker/` — cross-build and smoke-test assets
- `systemd/` — native service unit
- `scripts/` — installation helpers

## End-to-end deployment

### Build and deploy from source (recommended)

```bash
# Build, cross-compile, and push to a Pi Zero W in one step:
cargo xtask deploy --host pi@raspberrypi.local

# Same, with Zigbee coordinator support compiled in:
cargo xtask deploy --zigbee --host pi@raspberrypi.local

# Different board (Pi 4 64-bit):
cargo xtask deploy --target aarch64-unknown-linux-gnu --host pi@raspberrypi.local
```

`--host` is a `user@address` SSH destination. mDNS hostnames like `raspberrypi.local` work.

On first install `upgrade.sh` delegates to `install.sh` automatically — no separate
first-boot step required.

### Install from a pre-built release (no Rust required)

Download the tarball for your board from the [releases page](../../releases), then:

```bash
# Push the tarball and run the upgrade script on the device:
cargo xtask push --tarball ~/Downloads/home-edge-arm-unknown-linux-gnueabihf.tar.gz --host pi@192.168.1.50

# Roll back to the previous binary:
cargo xtask rollback --host pi@192.168.1.50
```

`cargo xtask` requires only `cargo`, `ssh`, and `scp`.

If you don't have Rust at all you can transfer and install manually:

```bash
scp home-edge-arm-unknown-linux-gnueabihf.tar.gz pi@192.168.1.50:/var/tmp/
ssh pi@192.168.1.50 '
  mkdir -p /var/tmp/home-edge-update &&
  tar -xzf /var/tmp/home-edge-update.tar.gz -C /var/tmp/home-edge-update &&
  sudo sh /var/tmp/home-edge-update/upgrade.sh
'
```

The device does not need internet access — everything is transferred over your local
network via SSH/SCP.

SSH timeout defaults are 10 s connect + 30 s stall tolerance (5 s × 6 probes).
Override with `--connect-timeout`, `--alive-interval`, `--alive-count`.

### Pairing the iOS / Android companion app

Home Edge advertises itself as `_home-assistant._tcp.local.` via mDNS, so the companion app
auto-discovers it on the same network. Tap **Add server**, select the device from the list,
and complete the standard onboarding flow (account creation → location → done).

### Release tarballs

| Tarball | Board | Notes |
|---|---|---|
| `home-edge-arm-unknown-linux-gnueabihf.tar.gz` | Raspberry Pi Zero W, Pi 1 | ARMv6, hard-float |
| `home-edge-armv7-unknown-linux-gnueabihf.tar.gz` | Raspberry Pi 2/3/4 (32-bit OS) | ARMv7 |
| `home-edge-aarch64-unknown-linux-gnu.tar.gz` | Raspberry Pi 3/4/5 (64-bit OS), most SBCs | AArch64 |
| `home-edge-riscv64gc-unknown-linux-gnu.tar.gz` | StarFive VisionFive 2, Milk-V Pioneer | RISC-V 64 |

## Zigbee setup

Enable the `zigbee` feature at compile time and add a `[zigbee]` section to `config.toml`.

### CH340 USB dongle on Pi Zero W (usbdevfs)

The Pi Zero W's dwc2 USB host controller cannot handle concurrent Bulk-IN/OUT URBs
through the kernel `ch341` serial driver. Home Edge uses direct `usbdevfs` access to
bypass the kernel driver entirely.

Run `lsusb` on the device to find the bus and device numbers (usually `001/002` for
the first USB device):

```
Bus 001 Device 002: ID 1a86:7523 QinHeng Electronics CH340 serial converter
```

```toml
# config.toml
[zigbee]
serial_port = "/dev/bus/usb/001/002"   # usbdevfs path from lsusb
baudrate    = 115200
```

### Standard serial coordinator (ttyACM0 / ttyUSB0)

For boards without the dwc2 limitation:

```toml
[zigbee]
serial_port = "/dev/ttyACM0"
baudrate    = 115200
# adapter = "znp"     # znp | ezsp | auto (default: znp)
# channel = 11        # Zigbee channel 11–26 (default: 11)
```

Deploy with Zigbee support compiled in:

```bash
cargo xtask deploy --zigbee --host pi@raspberrypi.local
```

Once running, open `http://<device-ip>:8124/zigbee` to manage devices and permit joining.

## Configuration reference

```toml
[server]
host      = "0.0.0.0"
port      = 8124
# log_level = "info"   # RUST_LOG env var takes precedence

[storage]
data_dir = "/var/lib/home-edge"

[ui]
product_name = "Home Edge"

# Seed the area registry on first boot only.
[areas]
names = ["Living Room", "Kitchen", "Bedroom"]

# Pre-configure the home zone (optional; onboarding values take precedence after first boot).
# [home_zone]
# latitude  = 51.5074
# longitude = -0.1278
# radius    = 100.0

# History ring-buffer: max sensor readings retained per entity.
# [history]
# capacity = 1000

# mDNS — skip USB tethering/gadget interfaces not reachable by LAN clients.
[mdns]
exclude_interfaces = ["usb", "rndis", "ncm"]

# Zigbee coordinator (optional).
# [zigbee]
# serial_port = "/dev/ttyACM0"
# baudrate    = 115200
```

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

### Build options

```bash
# Build a deployment tarball (no deploy):
cargo xtask package
cargo xtask package --target aarch64-unknown-linux-gnu
cargo xtask package --zigbee
# → home-edge-<target>.tar.gz
```

See `cargo xtask --help` and `cargo xtask <command> --help` for all options.

## API surface

### HA-compatible REST

| Method | Path | Description |
|---|---|---|
| `GET` | `/api/` | API status |
| `GET` | `/api/config` | Instance configuration |
| `GET` | `/api/states` | All entity states |
| `GET/POST` | `/api/states/{entity_id}` | Get or set a single state |
| `GET` | `/api/services` | Available services |
| `POST` | `/api/services/{domain}/{service}` | Call a service |
| `GET` | `/api/config/device_registry/list` | Device registry |

### HA-compatible WebSocket (`/api/websocket`)

Full auth + subscribe/call protocol. Used by the iOS and Android companion apps.

### Mobile integration

| Method | Path | Description |
|---|---|---|
| `POST` | `/api/mobile_app/registrations` | Register a companion device |
| `POST` | `/api/webhook/{webhook_id}` | Webhook callbacks from companion apps |

### Onboarding

| Method | Path |
|---|---|
| `GET` | `/api/onboarding` |
| `POST` | `/api/onboarding/users` |
| `POST` | `/api/onboarding/core_config` |
| `POST` | `/api/onboarding/analytics` |
| `POST` | `/api/onboarding/integration` |
| `POST` | `/api/onboarding/complete` |

### Zigbee (requires `--features zigbee`)

| Method | Path | Description |
|---|---|---|
| `GET` | `/api/zigbee/devices` | List paired devices |
| `PATCH` | `/api/zigbee/devices/{ieee}` | Rename a device |
| `DELETE` | `/api/zigbee/devices/{ieee}` | Remove a device |
| `POST` | `/api/zigbee/permit_join` | Open permit-join window |
| `POST` | `/api/zigbee/permit_join/stop` | Close permit-join window |
| `GET` | `/api/zigbee/permit_join/status` | Permit-join status |

### Edge-internal

| Method | Path | Description |
|---|---|---|
| `GET` | `/api/health` | Health check |
| `GET` | `/api/edge/history/{entity_id}` | Per-entity history ring-buffer |
| `POST` | `/api/system/restart` | Graceful restart (exit 100 → systemd restarts) |

### Web UI pages

`/`, `/onboarding`, `/devices`, `/areas`, `/zones`, `/history`, `/logbook`,
`/developer-tools`, `/notifications`, `/system`, `/zigbee` (when compiled in)
