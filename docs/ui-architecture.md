# Home Edge — UI Architecture & HA Faithfulness Plan

> Generated: 2026-04-11  
> Covers: HA iOS companion app, HA core (2026.4 dev), and home-edge current state.

---

## 0. Key Constraint: BLE Transport Compatibility

**This is the primary architectural driver.** home-edge compiles as exactly one of two mutually exclusive transport builds. Everything in the UI plan must be evaluated against both:

| Axis | `transport_wifi` | `transport_ble` |
|---|---|---|
| HTTP server | ✅ Axum, port 8124 | ❌ None — `run()` immediately returns an error |
| Protocol | HA-compatible REST + WebSocket | `BleCompactProtocol` — custom compact binary GATT |
| HA iOS `HAKit` WebSocket | ✅ Full auth + subscriptions | ❌ Does not apply |
| Live state reads | ✅ (`live_allowed: true`) | ❌ (`live_allowed: false`) — cached/stale only |
| State streaming (subscribe_entities) | ✅ Full diffs | ❌ `EventPolicy::MinimalNotifications` only |
| Paging required | ❌ Optional | ✅ Mandatory |
| Max entities per page | 512 | **32** |
| Service calls (writes) | Immediate | Wake-required — `WakeForCommands` / `writes_require_wake: true` |
| Authentication | OAuth2 / bearer tokens | BLE bonded session (`AuthPolicy::BondedSession`) |
| mDNS-SD advertisement | ✅ `_home-assistant._tcp.local.` | ❌ BLE advertising only |
| Web UI templates | ✅ Minijinja served over HTTP | ❌ Unreachable — no HTTP |

### 0.1 What This Means for the UI Surface

Because the BLE build has **no HTTP server**, the current web templates physically cannot render on a BLE-connected device. The UI rendering must happen **elsewhere**:

1. **Native iOS layer** — the iOS companion app must serve as the UI for BLE devices. SwiftUI views talking directly over CoreBluetooth to the `BleCompactProtocol` GATT service. This maps precisely to what the app already does for Watch/Widgets/CarPlay (native surfaces calling a compact API, not the WKWebView dashboard).

2. **WiFi relay** — a phone provides the BLE bridge and proxies/translates to a thin web UI served from the phone itself. This adds unnecessary complexity and defeats low-power goals.

3. **Option 1 is the right answer**: the native iOS app becomes the primary UI for BLE home-edge, using the same `WatchMagicViewRow`-style list components it already ships.

### 0.2 BLE Transport Policy (from `core.rs` `PolicyResolver`)

```
BleOperational = {
  live_allowed:        false      ← no fresh reads; always serve cached
  cached_allowed:      true
  stale_allowed:       true       ← age indicator required in UI
  paging_required:     true
  max_page_size:       32         ← hard ceiling per request
  max_event_batch:     16
  writes_allowed:      true
  writes_require_wake: true       ← must wake device before command executes  
  event_policy:        MinimalNotifications   ← notification IDs only, no diffs
  compatibility_policy: BleCompactProtocol    ← not HA REST/WS
  power_policy:        WakeForCommands
  auth_policy:         BondedSession
}
```

### 0.3 What CAN Be Faithfully Approximated Over BLE (Native iOS)

| HA UI feature | BLE approximation | Notes |
|---|---|---|
| Entity tile list (overview) | Paginated SwiftUI `List` (≤32) | `WatchMagicViewRow` is already this pattern |
| State display (on/off/value) | Cached state + "stale age" badge | Show `age_ms` from `FreshnessInfo` |
| Toggle / service call | "Waking…" spinner → execute → confirm | 3-state: pending-wake → executing → done |
| Binary sensor on/off | Colored tile, no streaming | Refresh on appear or manual pull |
| Sensor numeric value | Value + unit, cached | Staleness indicator if `age_ms > threshold` |
| Area grouping | Sort list by area, still paginated | Same groups, smaller page |
| Assist (voice) | WatchAssistView via BLE audio relay | Depends on BLE bandwidth; may need companion relay |
| Onboarding | Native claim flow (`AuthPolicy::OnboardingClaim`) | No OAuth2; BLE bond replaces |
| Areas list | GATT read paginated | Up to 32 areas per page |

### 0.4 What CANNOT Work Over BLE (or needs major redesign)

| HA UI feature | Why it can't work as-is |
|---|---|
| Web UI (any HTML template) | No HTTP server in BLE build |
| `subscribe_entities` live diffs | `EventPolicy::MinimalNotifications` — no stream |
| Dashboard editor / Lovelace | Too large; no HTTP; not in BLE scope |
| Camera streams (HLS/WebRTC) | Live data; bandwidth prohibitive over BLE |
| History graphs (rich) | High data volume; not in BLE protocol |
| mDNS discovery by iOS app | BLE advertising replaces mDNS |
| Full OAuth2 flow | BLE uses bonded session, not token exchange |
| Logbook / developer tools | WiFi-only; not in compact protocol |

### 0.5 Shared Design Principles (apply to BOTH transports)

These principles must govern every UI decision so the same visual language works on both surfaces:

1. **Cache-first display**: always show last known value + age; never block on a live read.
2. **32-entity pagination**: every view that lists entities must page at ≤32. WiFi can fetch 512 but the visual components must not assume more than 32 per visible page.
3. **3-state command UX**: `idle → pending/waking → confirmed`. WiFi commands resolve fast; BLE commands may take 1–3 s to wake. The component must handle both.
4. **Stale indicators**: any value older than a configurable threshold (e.g. 60 s) shows a visual staleness badge. This is a no-op on WiFi (always fresh) but essential on BLE.
5. **Pull-to-refresh as primary refresh**: no reliance on streaming push in the base components; streaming is an optional enhancement layered on top.
6. **No-JS / minimal-JS for web tier**: the web templates must function (read-only) without JavaScript (streaming/realtime is JS-enhancement). This keeps the WiFi web UI compatible with being rendered inside a BLE-connected relay client with limited JS execution.
7. **Domain-minimal attributes**: entity card components should work with just `{entity_id, state, friendly_name, icon}` — the minimum the BLE compact protocol delivers. Richer attributes (RGB, forecast data) are progressive enhancements.

---

## 1. The HA UI Model — What We're Recreating

Home Assistant's UI is split into two distinct rendering tiers:

| Tier | Technology | Managed by |
|---|---|---|
| **Shell** (sidebar, nav, auth gate) | Custom web components (`ha-sidebar`, `ha-app-layout`) in Lit/Polymer | HA frontend JS bundle |
| **Dashboard / panels** | Lovelace cards rendered inside the SPA | Frontend + custom cards |

The **iOS companion app** wraps this entirely in a `WKWebView` — the native app renders **zero** Lovelace content natively. The web frontend is the sole dashboard surface. Native layers (Watch, Widgets, CarPlay, Controls) are separate surfaces that re-use the same HA WebSocket API and entity model.

Home Edge must therefore own the full rendering stack: shell + all card types. There is no HA SPA to delegate to.

---

## 2. Full Screen / Panel Inventory

### 2.1 Core Sidebar Panels (every HA installation)

| Panel (`url_path`) | Sidebar icon (MDI) | Web component | Native equivalent |
|---|---|---|---|
| `lovelace` | `mdi:view-dashboard` | `ha-panel-lovelace` | — (WebView in iOS) |
| `history` | `mdi:chart-box` | `ha-panel-history` | — |
| `logbook` | `mdi:format-list-bulleted-type` | `ha-panel-logbook` | — |
| `map` | `mdi:tooltip-account` | `ha-panel-map` | — |
| `developer-tools` | `mdi:hammer` | `ha-panel-developer-tools` | — |
| `config` | `mdi:cog` | `ha-panel-config` | Settings screens in iOS |
| `profile` | `mdi:account` | `ha-panel-profile` | Settings → General in iOS |

Additional panels are registered by integrations (e.g., `energy`, `todo`, `shopping_list`).

### 2.2 Config Sub-Panels (Settings area)

Within `/config` the HA frontend renders a Settings SPA with sections:

| Section | Path | Description |
|---|---|---|
| Devices & Services | `/config/integrations` | Manage integrations |
| Automations | `/config/automations` | Automation CRUD |
| Scenes | `/config/scenes` | Scene management |
| Scripts | `/config/scripts` | Script management |
| Areas & Zones | `/config/areas` | Area/zone registry |
| People | `/config/users` | User management |
| System | `/config/system` | Logs, restart, backups |
| Dashboard | `/config/lovelace/dashboards` | Multi-dashboard management |

---

## 3. The Lovelace / Dashboard System

### 3.1 View Types

A dashboard has one or more **views**; each view has a `type`:

| View type | Layout | Notes |
|---|---|---|
| `masonry` (default) | Pinterest-style columns | Column count auto-scales by screen width |
| `sidebar` | Left sidebar (wide) + masonry main | Fixed sidebar width, rest masonry |
| `panel` | Single card fills full view | Usually one `map` or `picture-element` |
| `sections` | Ordered horizontal columns | New in 2024.x |
| `grid` | CSS grid with explicit positions | Manual placement |

Views also have **badges** — small inline chips in the view header showing entity state (typically sensors or binary sensors).

### 3.2 Card Type Catalog (priority order for home-edge)

Cards are **opaque JSON objects** - the `type` string is interpreted by the frontend. Core cards:

#### Tier 1 — Universal / Most-used

| Card type | What it shows | Key config fields | Interactions |
|---|---|---|---|
| `entities` | List of entity rows within a card | `entities[]` (entity_id or row config), `title`, `show_header_toggle` | Row tap → more-info dialog; header toggle turns all on/off |
| `tile` | Single entity as a large tap target | `entity`, `name`, `icon`, `color`, `features[]` | Tap toggles on/off or opens more-info |
| `button` | Single actionable entity as a pressable card | `entity`, `name`, `icon`, `tap_action`, `hold_action` | Tap action (call service / navigate / etc.) |
| `history-graph` | Time-series line graph | `entities[]`, `hours_to_show` | Hover for value at time |
| `picture-entity` | Entity overlaid on an image | `entity`, `image` URL, `show_state`, `show_name` | Tap → more-info |
| `picture-glance` | Image with entity icon badges | `entities[]`, `image`, `camera_image` | Entity badge tap → more-info |
| `glance` | Grid of compact entity tiles | `entities[]`, columns | Tile tap → more-info |
| `media-control` | Full media player card | `entity` (media_player domain) | Play/pause/seek/volume/source |
| `thermostat` | Climate control ring | `entity` (climate domain) | Drag ring to set temp, mode button |
| `light` | Light control card | `entity` (light domain) | Brightness slider, color picker |
| `map` | Live location map | `entities[]` (person/device_tracker), `geo_location_sources[]` | Pan/zoom |

#### Tier 2 — Commonly Used

| Card type | What it shows |
|---|---|
| `alarm-panel` | Alarm keypad + arm/disarm buttons |
| `weather-forecast` | Current + hourly/daily forecast; icon + temperature |
| `energy` | Energy dashboard embed |
| `logbook` | Event history for entities |
| `sensor` | Single sensor with optional graph |
| `gauge` | Radial gauge for numeric sensor |
| `todo-list` | To-do list checkbox UI |
| `shopping-list` | Shopping list checkboxes |
| `calendar` | Calendar view of upcoming events |
| `markdown` | Rendered markdown text |
| `picture` | Static image |
| `iframe` | Embedded iframe |
| `plant-status` | Plant sensor card  |
| `statistics-graph` | Long-term statistics bar/line chart |
| `custom:*` | Third-party / HACS custom cards |

#### Tier 3 — Admin / Advanced

| Card type | Notes |
|---|---|
| `conditional` | Wraps another card, shows based on conditions |
| `filter` | Dynamically filtered entity list |
| `entity-filter` | Entity list filtered by state |
| `grid` | Masonry layout container card |
| `horizontal-stack` / `vertical-stack` | Card layout containers |
| `area` | Area overview card |

### 3.3 Entity Row Types (within `entities` card)

| Row type | Purpose |
|---|---|
| `default` | Auto-detected based on domain |
| `section` (divider) | Visual separator with optional label |
| `button` | Press button row |
| `toggle` | On/off toggle row |
| `slider` | Numeric slider row |
| `select` | Dropdown select row |
| `input-select` | input_select entity row |
| `scene` | Scene activate row |
| `script` | Script trigger row |
| `weblink` | External URL link row |
| `cast` | Chromecast target row |
| `text` | Read-only text row |
| `custom:*` | Custom row types |

### 3.4 Features (sub-controls within cards)

`tile` and some other cards accept `features[]` — additional interactive controls in the card footer:

| Feature | Domain | Controls |
|---|---|---|
| `light-brightness` | light | Brightness slider |
| `light-color-temp` | light | Color temperature slider |
| `light-color` | light | HSL color picker circle |
| `cover-position` | cover | Position slider |
| `cover-tilt` | cover | Tilt slider |
| `climate-hvac-modes` | climate | Mode chip buttons |
| `climate-preset-modes` | climate | Preset chip buttons |
| `climate-target-temperature` | climate | Target temp stepper |
| `fan-speed` | fan | Speed percentage slider |
| `fan-preset-modes` | fan | Preset mode chips |
| `media-player-media-browse` | media_player | Browse media button |
| `vacuum-commands` | vacuum | Start/stop/dock buttons |
| `alarm-modes` | alarm_control_panel | Arm mode chip buttons |
| `select` | select, input_select | Dropdown in card |
| `target-humidity` | humidifier | Humidity slider |
| `number` | number, input_number | Slider or input |

### 3.5 Tap Actions

All interactive elements support configurable `tap_action`, `hold_action`, `double_tap_action`:

| Action type | Effect |
|---|---|
| `more-info` (default) | Opens more-info dialog for the entity |
| `toggle` | Calls appropriate toggle service |
| `call-service` | Calls specified `{domain}.{service}` with data |
| `navigate` | Pushes a new route within the HA SPA |
| `url` | Opens an external URL |
| `assist` | Opens Assist pipeline dialog |
| `none` | No action |

---

## 4. The More-Info Dialog — Entity Detail Overlay

The **more-info dialog** is central to HA's UX. It is a modal sheet that slides up from the bottom (mobile) or a centered dialog (desktop), showing full entity detail. Every entity type has a specialized more-info component.

### 4.1 Universal Fields (all entity types)

- Entity name + state string
- Last changed / last updated relative time
- Quick history sparkline (24 h)
- Icon (domain-default or custom MDI)
- Badges: device class, area, integration
- "More info" link → logbook for this entity
- Three-dot menu: rename, disable, entity settings

### 4.2 Per-Domain More-Info Components

| Domain | More-info content |
|---|---|
| **light** | Color swatch (if RGB/HS/XY), Kelvin temp slider, brightness slider, effect dropdown, transition input, on/off button |
| **switch** / inputBoolean | Toggle, state history |
| **climate** | Temperature ring (current + target), HVAC mode chips, preset mode chip, fan mode chip, swing mode chip, humidity target |
| **cover** | Open/close/stop buttons, position slider (if supported), tilt slider (if supported) |
| **media_player** | Album art, title/artist, play/pause/skip/seek bar, volume slider, source selector, group members |
| **alarm_control_panel** | State badge, code keypad (if required), arm buttons (home/away/night/vacation/custom), disarm |
| **lock** | Lock/unlock/open buttons, code input (if required), changed-by, state |
| **fan** | On/off, speed percentage slider, preset chips, oscillate toggle, direction toggle |
| **vacuum** | State + battery, start/pause/stop/return-home buttons, locate, fan speed, status |
| **camera** | Live stream (HLS or WebRTC), snapshot download, recording indicator |
| **sensor** / binary_sensor | State value, device_class-aware unit and icon, 24 h history graph |
| **number** / input_number | Slider (if step/min/max configured), value display, submit |
| **select** / input_select | Dropdown of options, current value |
| **text** / input_text | Text input field, max length |
| **button** / input_button | "Press" button, last pressed time |
| **scene** | "Activate" button |
| **script** | "Run script" button, last triggered |
| **automation** | Enable/disable toggle, "Trigger now" button, last triggered, trace list |
| **timer** | Countdown display, start/pause/cancel/finish buttons |
| **input_datetime** | Date and/or time picker |
| **person** | Map showing latest location, source entities |
| **weather** | Forecast tabs (hourly / daily), wind speed, humidity, pressure |
| **update** | Current version / latest version, install button, release notes |
| **todo** | Checklist of items |
| **event** | Most recent event data |
| **image** | Full-size image display |

---

## 5. Sidebar Navigation — Detailed Structure

The HA sidebar (`ha-sidebar`) is a custom element with precise anatomy:

```
<ha-sidebar>
  ├── .menu (64px header)
  │     ├── <ha-icon-button> — hamburger toggle (collapses to icon-only)
  │     └── .title — location_name (instance name)
  │
  ├── .nav (scrollable panel list)
  │     └── <paper-listbox> / <ha-md-list>
  │           ├── [panel items, each]
  │           │     ├── <ha-svg-icon> (24px MDI)
  │           │     ├── <span class="item-text"> — panel title
  │           │     └── <span class="notification-badge"> — orange count (optional)
  │           └── [spacer (.spacer div, flex: 1)]
  │
  ├─── [after spacer — fixed bottom items]
  │     ├── Developer Tools (admin only)
  │     ├── Notifications link (bell icon, badge count)
  │     └── User item (.user)
  │           ├── <ha-user-badge> — coloured circle with initials
  │           └── <span class="item-text"> — username
  │
  └── Notifications drawer
        ├── <ha-notification-drawer> slides in from left
        └── Sorted by priority: persistent, firing → dismissible
```

**Collapsed state** (56px): text + badges hidden; items centered; circles for nav items.

**Panels order**: The sidebar order is user-configurable (drag-and-drop in HA frontend settings). `show_in_sidebar` / `sidebar_badge` control visibility.

---

## 6. iOS Companion App — What's Native vs. WebView

### Rendered in WKWebView (the entire HA SPA)
- All Lovelace dashboard views and cards
- History, Logbook, Map panels
- Developer Tools
- Most of Config panel

### Rendered Natively
| Surface | Framework | What it shows |
|---|---|---|
| Onboarding | SwiftUI | Welcome → server discovery → OAuth → permissions |
| Settings | SwiftUI + UIKit (Eureka) | Connection URLs, notifications, sensors, watch, widgets |
| Assist | SwiftUI | Voice/text chat, waveform animation, pipeline picker |
| Watch home | SwiftUI (watchOS) | `MagicItem` list — entities, scripts, scenes, actions, folders |
| Watch Assist | SwiftUI (watchOS) | Voice recording, transcript, response |
| iOS Widgets (WidgetKit) | SwiftUI | Entity tile grid, sensor values, gauge |
| iOS 18 Controls | SwiftUI (`ControlWidget`) | Toggle/button per entity |
| CarPlay | CPKit templates | Quick access, browse by area/domain, entity lists |

### Native ↔ WebView Bridge (ExternalBus)

The WebView sends messages to native via `window.webkit.messageHandlers.externalBus.postMessage(obj)`:

| Message type | Direction | Purpose |
|---|---|---|
| `connection-status` | WebView → Native | Connected / disconnected events |
| `config_screen/show` | WebView → Native | Open native settings |
| `bar_code/scan` | WebView → Native | Request barcode scanner |
| `tag/read` / `tag/write` | WebView → Native | NFC operations |
| `assist/show` | WebView → Native | Open Assist sheet |
| `camera/show` | WebView → Native | Open camera player |
| `entity/add_to` | WebView → Native | Add entity to widget/watch/carplay |
| `improv/scan` | WebView → Native | BLE device provisioning |
| `matter/commission` | WebView → Native | Matter device setup |
| `toast/show` / `toast/hide` | WebView → Native | Native toast (iOS 18+) |
| `haptic` | WebView → Native | Haptic feedback request |
| `updateThemeColors` | WebView → Native | Theme CSS vars → status bar style |

---

## 7. Entity Domain Visual Language

### State → Visual Mapping

| Domain | Active state | Icon (default MDI) | Active color | Inactive color |
|---|---|---|---|---|
| `light` | `on` | `mdi:lightbulb` → `mdi:lightbulb-off` | `#FDD835` (yellow) | secondary text |
| `switch` | `on` | `mdi:toggle-switch` → `mdi:toggle-switch-off` | primary-color | secondary text |
| `input_boolean` | `on` | `mdi:check-circle-outline` | primary-color | secondary text |
| `binary_sensor` | `on` | device_class icon | `state == 'on'` ? alert/success : secondary | secondary |
| `cover` | `open` | `mdi:window-shutter-open` → `mdi:window-shutter` | primary-color | secondary |
| `lock` | `locked` | `mdi:lock` → `mdi:lock-open` | secondary (safe=locked) | `#F44336` (unlocked = alert) |
| `alarm_control_panel` | `armed_*` | `mdi:shield-check` / `mdi:shield-off` | state-dependent | — |
| `climate` | `heat/cool/heat_cool` | `mdi:thermostat` | `#F57C00` (heat) / `#0277BD` (cool) | secondary |
| `media_player` | `playing` | `mdi:cast-connected` | primary-color | `mdi:cast` |
| `fan` | `on` | `mdi:fan` (animated spin) | primary-color | secondary |
| `vacuum` | `cleaning` | `mdi:robot-vacuum` | primary-color | secondary |
| `sensor` | any | device_class icon | primary-text (value) | — |
| `person` | `home` | `mdi:account` | `#4CAF50` (home) | secondary (away) |
| `weather` | any | condition icon | — | — |
| `camera` | `streaming` | `mdi:video` | primary-color | secondary |
| `automation` | `on` | `mdi:robot` | primary-color | secondary |
| `script` | `off` (idle) | `mdi:script-text` | primary | — |
| `scene` | — | `mdi:palette` | — | — |
| `button` | — | domain-icon | — | — |
| `update` | `on` (update available) | `mdi:package-up` | `#FF9800` (warning) | secondary |

### Binary Sensor Device-Class Icons

| Device class | `on` icon | `off` icon | `on` meaning |
|---|---|---|---|
| `door` | `mdi:door-open` | `mdi:door-closed` | open |
| `window` | `mdi:window-open` | `mdi:window-closed` | open |
| `motion` | `mdi:motion-sensor` | `mdi:motion-sensor-off` | detected |
| `presence` / `occupancy` | `mdi:home` | `mdi:home-outline` | present |
| `smoke` | `mdi:smoke-detector-alert` | `mdi:smoke-detector` | detected |
| `moisture` | `mdi:water` | `mdi:water-off` | wet |
| `battery` | `mdi:battery-alert` | `mdi:battery` | low |
| `battery_charging` | `mdi:battery-charging` | `mdi:battery` | charging |
| `connectivity` | `mdi:check-network` | `mdi:close-network` | connected |
| `garage_door` | `mdi:garage-open` | `mdi:garage` | open |
| `lock` | `mdi:lock-open` | `mdi:lock` | unlocked |
| `plug` / `power` | `mdi:power-plug` | `mdi:power-plug-off` | plugged |
| `problem` / `safety` | `mdi:alert-circle` | `mdi:check-circle` | problem |
| `tamper` | `mdi:alert` | `mdi:check` | tampered |
| `vibration` | `mdi:vibrate` | `mdi:vibrate-off` | vibrating |
| `sound` | `mdi:music-note` | `mdi:music-note-off` | detected |
| `cold` | `mdi:snowflake` | `mdi:thermometer` | cold |
| `heat` | `mdi:fire` | `mdi:thermometer` | hot |
| `gas` | `mdi:gas-cylinder` | `mdi:gas-cylinder` | detected |
| `running` | `mdi:play` | `mdi:stop` | running |
| `update` | `mdi:package-up` | `mdi:package` | update available |

---

## 8. Flows — User Interaction Journeys

### 8.1 First-Run / Onboarding Flow

```
App open (no server)
  └─ /onboarding
       Step 1: User creation       POST /api/onboarding/users
       Step 2: Location/units      POST /api/onboarding/core_config
       Step 3: Analytics           POST /api/onboarding/analytics
       Step 4: Integration         POST /api/onboarding/integration
       Step 5: Complete            POST /api/onboarding/complete
         └─ Redirect to /auth/authorize
              └─ OAuth2 code → POST /auth/token
                   └─ Dashboard (/)
```

**iOS companion specific**: After OAuth the flow continues into native post-steps: device naming, sensor registration, mobile_app registration (`POST /api/mobile_app/registrations`), push notification setup.

### 8.2 Authentication Flow (existing user / page reload)

```
Browser → GET /
  └─ Token present in localStorage? → Load dashboard
  └─ No token →
       GET /auth/authorize?client_id=...&redirect_uri=...&state=...
         └─ POST /auth/login_flow       → {flow_id}
         └─ POST /auth/login_flow/{id}  with {username, password}
              → {result: "create_entry", auth_code: "..."}
         └─ POST /auth/token            with code
              → {access_token, refresh_token, expires_in: 1800}
         └─ Redirect to dashboard with token stored
```

### 8.3 Entity Control Flow

```
User taps entity card / tile
  ├─ Simple toggle (light, switch, fan): 
  │    WS call_service → {domain}.{toggle|turn_on|turn_off}
  │    State update arrives via subscribe_entities diff
  │    Card re-renders
  │
  └─ More-info dialog:
       WS get_states or cached state → render dialog
       User interaction (slider/button):
         WS call_service → state updates broadcast
         Dialog + card re-render from state diff
```

### 8.4 Dashboard Configuration Flow

```
/config/lovelace/dashboards (admin)
  ├─ List: WS lovelace/dashboards/list
  ├─ Edit: WS lovelace/config (load) → visual editor → WS lovelace/config/save
  └─ Create: WS lovelace/dashboards/create
```

### 8.5 Area Browser Flow

```
Config → Areas & Zones (WS config/area_registry/list)
  ├─ Area card → entity list for that area (filter by area_id)
  └─ New area: WS config/area_registry/create {name}
```

---

## 9. What Is Configurable vs. Hardcoded

### Configurable by User (in HA frontend)

| Feature | Where | Storage |
|---|---|---|
| Dashboard layout (cards, views, order) | Lovelace editor | `.storage/lovelace*` |
| Dashboard theme | Profile → Theme | `.storage/frontend_theme` |
| Sidebar panel order / visibility | Settings → Dashboard → Sidebar | `.storage/lovelace` |
| Area assignments | Settings → Areas | `.storage/core.area_registry` |
| Entity name overrides | Settings → Entities | `.storage/core.entity_registry` |
| Automations, scripts, scenes | Config sections | `.storage/automation*`, etc. |
| Users + permissions | Settings → People → Users | `.storage/auth*` |
| Extra JS/CSS resources | Config → Lovelace Resources | `.storage/lovelace_resources` |

### Configurable in iOS App (native settings)

| Setting | Configurable? |
|---|---|
| Server URL (internal + external) | ✅ User |
| Home SSID for internal URL | ✅ User |
| mTLS client certificate | ✅ User |
| Self-signed cert acceptance | ✅ User |
| Page zoom (50–200%) | ✅ User |
| Edge-to-edge / full-screen mode | ✅ User |
| Pull-to-refresh behaviour | ✅ User |
| Swipe gesture actions | ✅ User |
| Sensor update interval (20 s–1 h) | ✅ User |
| Enable/disable individual sensors | ✅ User |
| Location permission tier | ✅ User |
| Watch home item list | ✅ User |
| Widget item grids | ✅ User |
| CarPlay quick-access items | ✅ User |
| CarPlay domain list order | ✅ User |
| Kiosk mode / screensaver | ✅ User |
| App icon | ✅ User |
| Notification categories/actions | ✅ User |
| Push notification sounds | ✅ User |
| Firebase auto-init (privacy) | ✅ User |

### Hardcoded in iOS App

| Item | Value |
|---|---|
| User-agent token | `"Mobile/HomeAssistant"` — signals to HA frontend it's the app |
| CarPlay supported domains | `[light, button, cover, inputBoolean, inputButton, lock, scene, script, switch]` |
| Watch supported domains (customization) | `[script, scene]` |
| External bus message type strings | See §6 bridge table |
| App Group ID | `AppConstants.AppGroupID` |
| HA WebSocket protocol version string | `"2025.1.0"` (for auth handshake) |

---

## 10. Current home-edge State vs. HA Parity

> **Transport key**: 🔵 WiFi only | 🟠 BLE only | 🟢 Both / transport-agnostic

### Already Implemented ✅

| Feature | Status | Transport |
|---|---|---|
| HA-compatible mDNS-SD advertisement (`_home-assistant._tcp.local.`) | ✅ | 🔵 WiFi |
| Full HA OAuth2/IndieAuth auth flow (`/auth/login_flow`, `/auth/token`, `/auth/revoke`) | ✅ | 🔵 WiFi |
| HA WebSocket protocol (auth handshake, `ha_version`) | ✅ | 🔵 WiFi |
| `GET /api/`, `/api/config`, `/api/states`, `/api/states/{id}` | ✅ | 🔵 WiFi |
| `POST /api/states/{id}`, `GET /api/services`, `POST /api/services/{domain}/{service}` | ✅ | 🔵 WiFi |
| `WS: ping, get_states, get_config, get_services, call_service` | ✅ | 🔵 WiFi |
| `WS: subscribe_entities` (compressed diffs via broadcast channel) | ✅ | 🔵 WiFi |
| `WS: subscribe_events, unsubscribe_events` | ✅ | 🔵 WiFi |
| `WS: auth/current_user, get_panels` | ✅ | 🔵 WiFi |
| `WS: frontend/get_themes` (stub) | ✅ | 🔵 WiFi |
| `WS: config/area_registry/{list,create,update,delete}` | ✅ | 🔵 WiFi |
| `WS: config/device_registry/list` | ✅ | 🔵 WiFi |
| Mobile app registration (`POST /api/mobile_app/registrations`) | ✅ | 🔵 WiFi |
| Webhook sensor payload ingestion | ✅ | 🔵 WiFi |
| In-memory state store with broadcast push | ✅ | 🟢 Both (core) |
| Per-entity ring-buffer history (1000 entries) | ✅ | 🟢 Both (core) |
| Server-rendered web UI (Minijinja + HTMX) | ✅ | 🔵 WiFi |
| CSS design tokens matching HA frontend (light theme) | ✅ | 🔵 WiFi |
| HA-style sidebar (hamburger, icon-only collapse, mobile overlay) | ✅ | 🔵 WiFi |
| Sensor tile grid with HTMX polling | ✅ | 🔵 WiFi |
| Connected device cards with entity count | ✅ | 🔵 WiFi |
| Server-side SVG sparklines in history | ✅ | 🔵 WiFi |
| iOS external bus integration (connection-status, config_screen/show) | ✅ | 🔵 WiFi |
| `hassConnection` stub (suppresses iOS WebViewBridge spinner) | ✅ | 🔵 WiFi |
| Onboarding wizard (5-step: user, location, analytics, integration, complete) | ✅ | 🔵 WiFi |
| Area registry with TOML seed config | ✅ | 🟢 Both (core) |
| Atomic JSON persistence for devices, entities, areas, tokens, auth | ✅ | 🔵 WiFi |
| `RuntimeMode` + `TransportPolicy` framework (BLE modes modeled) | ✅ | 🟢 Both (core) |
| BLE operational + unprovisioned mode enums + policy constants | ✅ | 🟠 BLE (dormant) |

### Missing — High Priority (core experience)

| Feature | Gap | Transport | Effort |
|---|---|---|---|
| **BLE `run()` implementation** | `app.rs` BLE path immediately returns error; entire BLE transport unimplemented | 🟠 BLE | **XL** |
| **BLE GATT server + compact protocol** | No GATT characteristic layout; no BleCompactProtocol encoder/decoder | 🟠 BLE | **XL** |
| **Native iOS BLE UI** | No SwiftUI views for BLE home-edge; iOS app only uses HAKit WebSocket (WiFi) | 🟠 BLE | **XL** |
| **Lovelace dashboard config** — `lovelace/config`, `lovelace/config/save`, `lovelace/dashboards/*` WS commands | WS stubs missing | 🔵 WiFi | Medium |
| **Entity registry** — `entity_registry/{list,update}` WS commands | Not implemented | 🔵 WiFi | Medium |
| **More-info dialog** — per-domain entity detail modal | No UI component exists | 🔵 WiFi | Large |
| **Per-domain entity cards** — tile, entities, button, light, climate, cover cards | Only sensor tiles exist | 🔵 WiFi | Large |
| **Staleness age indicator** | No `FreshnessInfo` exposed to UI; no visual age badge | 🟢 Both | Small |
| **3-state command UX** (idle → waking → confirmed) | Commands have no pending state in current UI | 🟢 Both | Small |
| **32-entity pagination** in all list views | Current sensor grid renders unlimited entities | 🟢 Both | Small |
| **Dark mode** / CSS variable toggle | Only light theme | 🔵 WiFi | Small |
| **SSE stream** at `GET /api/stream` | Not implemented | 🔵 WiFi | Medium |
| **`POST /api/template`** endpoint | Not implemented | 🔵 WiFi | Small |

### Missing — Medium Priority (full fidelity)

| Feature | Gap | Transport | Effort |
|---|---|---|---|
| **Service call `target` expansion** — device_id/area_id → entity_ids | Only entity_id targets work | 🟢 Both | Medium |
| **Floor / label registry** WS commands | Not implemented | 🔵 WiFi | Small |
| **`config/entity_registry/*`** — rename/area assign/disable | Not implemented | 🔵 WiFi | Medium |
| **`auth/long_lived_access_token`** WS command | Not implemented | 🔵 WiFi | Small |
| **`auth/sign_path`** WS command (signed temp URLs) | Not implemented | 🔵 WiFi | Small |
| **Camera proxy** (`/api/camera_proxy/{id}`) | Not implemented | 🔵 WiFi | Medium |
| **History API** `/api/history/period` | Only custom `/api/history/{entity_id}` | 🔵 WiFi | Small |
| **BLE `MinimalNotification` → UI refresh bridge** | Notification delivery to native iOS not wired | 🟠 BLE | Medium |
| **BLE onboarding claim flow** | `AuthPolicy::OnboardingClaim` not implemented | 🟠 BLE | Large |

### Missing — Low Priority / Future

| Feature | Transport |
|---|---|
| Automations, scripts, scenes CRUD | 🔵 WiFi |
| Recorder long-term statistics | 🔵 WiFi |
| Logbook/History panels | 🔵 WiFi |
| Map panel (person tracking) | 🔵 WiFi |
| Notification delivery (push gateway) | 🟢 Both |
| Cloud / remote UI proxy | 🔵 WiFi |
| Multi-user support | 🔵 WiFi |
| MFA modules | 🔵 WiFi |
| BLE Assist relay (audio over GATT) | 🟠 BLE |

---

## 11. Recommended UI Implementation Plan for home-edge

### Phase 0 — BLE Transport Foundation (prerequisite for native BLE UI)

Before any BLE native UI work, the BLE transport layer itself must be built. Without this, the iOS app has nothing to talk to over CoreBluetooth.

#### 0.1 BLE GATT Server

The `transport_ble` `run()` function currently `bail!()`s immediately. It needs:

- A GATT server exposing at minimum:
  - **Service**: `home-edge-v1` (128-bit UUID)
  - **Auth characteristic**: bond + claim handshake (maps to `AuthPolicy::BondedSession` / `OnboardingClaim`)
  - **State read characteristic**: paginated entity state read with `PageRequest {limit: 32, cursor}` serialized via `BleCompactProtocol`
  - **Command write characteristic**: compact `OperationRequest::CallService` / `SetEntityState`
  - **Notification characteristic**: `DomainEvent::StateChanged` notification with `{entity_handle, revision}` — triggers client re-read; no payload diff (matching `EventPolicy::MinimalNotifications`)
  - **Config characteristic**: `ConfigSummary` (product name, mode)

#### 0.2 BleCompactProtocol Wire Format

Design as a compact binary format (e.g., MessagePack or a hand-rolled TLV). Constraints:
- BLE MTU is typically 20–517 bytes (ATT); use L2CAP CoC for larger payloads or split into 20-byte chunks with sequence numbers
- All entity states must fit in `max_page_size: 32` entries per read
- `FreshnessInfo.age_ms` and `FreshnessState` must be included in every state response (drives the staleness indicator in the UI)

#### 0.3 Native iOS BLE Client (in the iOS app)

A new `HomeEdgeBLEConnection` class alongside the existing `HAConnection` (HAKit) WebSocket:

```swift
// Mirrors HAConnection API surface where possible for component reuse

class HomeEdgeBLEConnection: ObservableObject {
    @Published var connectionState: BLEConnectionState
    func fetchStates(page: PageRequest) async throws -> [EntityStateView]
    func callService(_ call: ServiceCall) async throws -> ServiceOutcome
    func subscribeNotifications() -> AsyncStream<DomainEvent>
}
```

Native UI views for BLE (reuse existing iOS DesignSystem components):
- `BLEDeviceListView` — discovered peripherals (replaces mDNS scan)  
- `BLEEntityListView` — paginated entity list (≤32); reuses `WatchMagicViewRow`
- `BLEEntityDetailSheet` — acts like more-info dialog; shows cached state + 3-state command controls
- `BLEStatusBadge` — `FreshnessState` → "stale 3 min ago" badge
- `BLEWakeIndicator` — spinning indicator while `WakeForCommands` is in progress



The current dashboard only shows sensor tiles. The core UX improvement is a proper entity card system with:

> **BLE constraint**: All entity card components must satisfy the principles in §0.5. Each card must be renderable from a `{entity_id, state, friendly_name, icon, unit?, age_ms?}` payload — the minimum the compact protocol delivers. Richer attributes are progressive enhancements.

#### 11.1 Entity Card Component Architecture

Every card needs:
1. **State fetcher**: read from `/api/states/{entity_id}` (WiFi) or GATT cached read (BLE native)
2. **Renderer**: domain-aware HTML template (WiFi web) / SwiftUI view (BLE native)
3. **Action handler**: POST to `/api/services/{domain}/{service}` (WiFi) or compact GATT write (BLE)
4. **Real-time update**: HTMX SSE (WiFi) or pull-on-appear + notification-triggered refresh (BLE)

Suggested Minijinja macro structure:
```
templates/
  cards/
    _card_base.html        — wrapper with header + body
    tile.html              — large single-entity tap target
    entities.html          — entity row list
    light.html             — brightness/color control
    climate.html           — thermostat ring
    media_player.html      — media controls
    cover.html             — position slider
    alarm_panel.html       — keypad
    sensor.html            — value + graph
    history_graph.html     — time-series chart
  rows/
    _row_base.html         — row wrapper
    toggle.html            — on/off toggle row
    slider.html            — numeric slider row
    select.html            — dropdown row
    read_only.html         — static value row
  more_info/
    _dialog.html           — modal shell
    light.html             — light more-info
    climate.html           — climate more-info
    media_player.html      — media more-info
    cover.html
    lock.html
    alarm_panel.html
    fan.html
    vacuum.html
    camera.html
    sensor.html
    default.html           — generic fallback
```

#### 11.2 Real-Time Updates Strategy

Current home-edge uses HTMX polling every 5 s. For a HA-faithful feel on WiFi:

**Option A — Server-Sent Events (SSE)**:
- Add `GET /api/stream` (per HA spec: filtered by `state_changed`)
- Use HTMX `hx-ext="sse"` on the dashboard container
- Server pushes HTML fragments on state change = instant UI updates

**Option B — WebSocket HTMX extension**:
- HTMX has a `ws` extension; the existing `/api/websocket` could push HTML fragments
- More complex but avoids separate SSE endpoint

**Option C — Alpine.js + fetch polling** (no HTMX changes):
- Add [Alpine.js](https://alpinejs.dev/) for reactive state
- Cards bind to entity state via `x-data` + interval polling `/api/states/{entity_id}`
- Simpler start; upgrade to push later

**Recommendation**: Implement SSE (`GET /api/stream`) first — it's already in the HA API spec, one endpoint, and HTMX has native SSE support.

> **BLE constraint**: SSE/WebSocket streaming is WiFi-only. On BLE, state updates arrive as `MinimalNotifications` (opaque change signals with entity handle + revision counter). The native iOS layer should treat a notification as a signal to re-fetch the cached state of the flagged entity via a GATT read — not as a diff payload. The 3-state command UX (§0.5 point 3) must be implemented in the native layer regardless of whether the WiFi web UI uses SSE or polling.

#### 11.3 More-Info Dialog

The more-info dialog is the most impactful missing component. Implementation pattern for WiFi web:

```html
<!-- HTMX: load more-info content into a dialog on entity tap -->
<div class="ha-card sensor-tile"
     hx-get="/fragments/more-info/{{ entity_id }}"
     hx-target="#more-info-dialog .dialog-content"
     hx-swap="innerHTML"
     onclick="document.getElementById('more-info-dialog').showModal()">
```

```rust
// New route in http.rs
async fn more_info_fragment(
    Path(entity_id): Path<String>,
    State(app): State<AppState>,
) -> Html<String> {
    let state = app.state_store.get(&entity_id).await;
    let domain = entity_id.split('.').next().unwrap_or("unknown");
    // render domain-specific more-info template
}
```

> **BLE native equivalent**: The `WatchMagicViewRow` confirmation dialog in the iOS Watch extension is the BLE analog of the more-info dialog. The pattern is: tap row → show detail sheet with cached state + available actions → user confirms → 3-state wake/execute/done. Domain-specific detail sheets should mirror the WiFi more-info content while satisfying the 32-entity-page and wake-required constraints.

#### 11.4 Lovelace Dashboard Config WS Commands

To support the iOS app's internal calls (and potentially a future dashboard editor):

```rust
// In ha_ws.rs register:
"lovelace/info"             → return {mode: "storage"}
"lovelace/config"           → return current dashboard JSON from storage
"lovelace/config/save"      → admin: persist new dashboard config
"lovelace/dashboards/list"  → return [{id: "lovelace", title: "Overview", ...}]
```

Storage: add a `dashboard_config.json` to the var dir (default: single view with auto-generated entity cards).

### Phase 2 — Dashboard Configuration & Panels

#### 11.5 Dashboard Auto-Generation

When no `dashboard_config.json` exists, auto-generate a dashboard from current state:
- Group entities by area → one "masonry" view section per area
- Within each area, render entity cards by domain priority: lights → switches → climate → covers → media players → sensors
- Sort sensors by device class (temperature, humidity, battery last)

#### 11.6 Sidebar Panel Navigation

Add these panels (all are stubs initially, with placeholder pages):
- `/history` — entity history browser (table + graph)
- `/logbook` — event log
- `/config` → `/config/areas` (area CRUD, used by iOS area assignment)
- `/config/entities` (entity rename / area / disable)
- `/config/users` (user management, initially read-only)
- `/config/lovelace` (dashboard editor — long-term)

#### 11.7 Auth Panel Improvements

- Add `WS: auth/long_lived_access_token` — required for scripts and some integrations
- Add `WS: auth/refresh_tokens` / `auth/delete_refresh_token`
- Add `WS: auth/sign_path` — required for camera proxy temp URLs

### Phase 3 — Look & Feel Parity

#### 11.8 Dark Mode

Add a `prefers-color-scheme: dark` block to `_css.html` with HA's dark theme tokens:
```css
@media (prefers-color-scheme: dark) {
  :root {
    --primary-color:              #03a9f4;
    --sidebar-background-color:   #1c1c1c;
    --page-bg:                    #111111;
    --card-bg:                    #1e1e1e;
    --primary-text:               #e1e1e1;
    --secondary-text:             #9e9e9e;
    --divider-color:              rgba(255,255,255,.12);
    --border:                     rgba(255,255,255,.12);
    --input-border:               rgba(255,255,255,.2);
  }
}
```

Also support `updateThemeColors` external bus message to override CSS vars at runtime from the iOS theme switcher.

#### 11.9 MDI Icon Completeness

Current `_icons.html` has ~20 inline SVG symbols. Needed additions for full domain coverage:
`lightbulb-off`, `thermostat`, `window-shutter`, `window-shutter-open`, `lock-open`, `robot-vacuum`, `cast-connected`, `cast`, `fan`, `fire`, `snowflake`, `shield-check`, `shield-off`, `account`, `package-up`, `robot`, `script-text`, `palette`, `door-open`, `door-closed`, `motion-sensor`, `smoke-detector`, `water`, `battery-alert`, `check-network`, `garage`, `garage-open`, `vibrate`, `music-note`, `play`, `stop`, `chart-box`, `format-list-bulleted`, `tooltip-account`, `hammer`, `cog`, `account`, `bell`, `plus`, `check`, `close`, `chevron-right`, `pencil`, `delete`, `information`, `alert-circle`, `view-dashboard-variant`.

#### 11.10 Responsive Grid (Masonry-like)

HA uses a responsive column layout for cards. Implementation:
```css
.lovelace-view {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
  gap: 8px;
  padding: 8px;
  align-items: start;    /* masonry-like: cards don't stretch */
}
```
For true masonry (different card heights), use CSS `grid-auto-rows: 1px` with JS row-span calculation, or simply use column-count CSS masonry (now widely supported).

---

## 12. HA Wire Format Reference (pinned for home-edge compatibility)

### Entity State Wire Shape
```json
{
  "entity_id": "light.living_room",
  "state": "on",
  "attributes": {
    "brightness": 200,
    "color_mode": "brightness",
    "friendly_name": "Living Room Light",
    "supported_color_modes": ["brightness"],
    "supported_features": 0
  },
  "last_changed": "2026-04-11T12:00:00.000000+00:00",
  "last_reported": "2026-04-11T12:00:00.000000+00:00",
  "last_updated":  "2026-04-11T12:00:00.000000+00:00",
  "context": {"id": "01JD...", "parent_id": null, "user_id": null}
}
```

### Service Call Response (WS)
```json
{"id": 5, "type": "result", "success": true,
 "result": {"context": {"id": "01JD...", "parent_id": null, "user_id": null}}}
```

### subscribe_entities Initial Snapshot
```json
{"id": 1, "type": "event", "event": {
  "a": {
    "light.living_room": {
      "s": "on",
      "a": {"brightness": 200, "friendly_name": "Living Room"},
      "lc": 1744372800.0,
      "lu": 1744372800.0,
      "c": "01JD..."
    }
  }
}}
```

### subscribe_entities State Change Diff
```json
{"id": 1, "type": "event", "event": {
  "c": {
    "light.living_room": {"+": {"s": "off", "lc": 1744372900.0}}
  }
}}
```

### config/area_registry/list Response
```json
{"id": 2, "type": "result", "success": true, "result": [
  {"area_id": "living_room", "name": "Living Room", "aliases": [], "floor_id": null, "icon": null, "picture": null, "labels": []}
]}
```

### lovelace/config Response
```json
{"id": 3, "type": "result", "success": true, "result": {
  "title": "Home Edge",
  "views": [{
    "title": "Overview",
    "path": "overview",
    "type": "masonry",
    "badges": [],
    "cards": []
  }]
}}
```

---

## 13. Design Tokens — HA Default Theme (CSS Variables)

The following variables are expected by any component that has been injected into the iOS WKWebView and is compatible with HA's theming API:

```css
/* Required by HA frontend + companion app theme extraction */
--primary-color:                   #03a9f4;
--accent-color:                    #ff9800;
--primary-background-color:        #fafafa;
--secondary-background-color:      #e5e5e5;
--card-background-color:           #ffffff;
--primary-text-color:              #212121;
--secondary-text-color:            #727272;
--disabled-text-color:             #bdbdbd;
--divider-color:                   rgba(0,0,0,.12);
--sidebar-background-color:        #ffffff;
--sidebar-text-color:              #212121;
--sidebar-icon-color:              rgba(0,0,0,.54);
--sidebar-selected-icon-color:     #0288d1;
--sidebar-selected-background-color: rgba(3,169,244,.12);
--app-header-background-color:     #03a9f4;
--app-header-text-color:           #ffffff;
--label-badge-background-color:    #03a9f4;
--label-badge-text-color:          #ffffff;
--switch-checked-button-color:     #ffffff;
--switch-checked-track-color:      #03a9f4;
--mdc-theme-primary:               #03a9f4;  /* Material Web Components */
--mdc-theme-secondary:             #ff9800;
--statusbar-color:                 #03a9f4;  /* iOS status bar */
--header-height:                   56px;
```

The iOS companion reads `statusbar-color` from the `updateThemeColors` bus message to set its native status bar tint.
