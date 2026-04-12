# home-edge TODO

Tracks known issues, follow-up work, and the remaining UX parity roadmap.
Items are grouped by area; the wave plan at the bottom drives PR sequencing.

---

## Inline code issues (from TODO/FIXME comments)

### `ha_auth.rs` — TokenStore

- [ ] **Hardcoded `tokens.json` path** (`load_persisted` line 119, `save_persisted` line 93)
      — derive from a `StorageConfig` constant or method instead of `.join("tokens.json")`
- [ ] **`auth_codes` never expire** — stale one-time codes accumulate in memory indefinitely;
      add a timestamp and a periodic sweep, or use a bounded LRU map
- [ ] **`TokenStoreInner` uses `RwLock` unnecessarily** — every mutation path takes a write lock,
      so the read-biased split provides no benefit; replace with `Mutex<TokenStoreInner>`
- [ ] **`Uuid::new_v4().to_string()` allocates on every token issue** — consider keeping a
      `[u8; 16]` or `Uuid` value rather than calling `to_string()` until serialisation
- [ ] **Client-id mismatch on `exchange_code` / `refresh_token` is silently `None`** — should
      be logged at `warn!` to aid debugging of mis-configured companion apps

---

## Webhook stubs (accepted but not implemented)

- [ ] **`update_location`** — device GPS/location update ack'd with `{}` but not stored;
      needs a location store and presence-detection logic
      (source: `mobile_app/webhook.py handle_webhook_update_location`)
- [ ] **`fire_event`** — ack'd with `{}` but not dispatched; needs an event bus
      (source: `mobile_app/webhook.py handle_webhook_fire_event`)
- [ ] **`get_zones`** — always returns `[]`; implement zone configuration and persistence
      (source: `mobile_app/webhook.py handle_webhook_get_zones`)
- [ ] **`get_config` → `user_id`** — currently `null`; wire up authenticated user id once
      multi-user support lands

---

## Onboarding / auth

- [ ] **`POST /api/onboarding/integration/wait`** — currently a stub returning
      `{"integration_loaded": true}` immediately; no action needed for embedded device
      but may need real signalling if integration loading becomes async
- [ ] **Access token expiry not enforced at the HTTP layer** — `TokenStore` issues tokens
      but has no TTL check on `validate_access_token`; add issued-at timestamp and a
      1800 s expiry window (aligns with `fix/token-expiry` PR intent)

---

## UX parity — Wave plan

Wave 1 is complete and merged. Remaining waves are gated as shown.

### Wave 2 — scaffold (needs Wave 1 merged ✅)

- [x] **`feat/card-templates-scaffold`** — complete (merged into `feat/dashboard-cards`)
  - `crates/controller/templates/more_info/` directory + 11 domain templates + `_default.html`
  - `<dialog id="more-info-dialog">` shell added to `_base_app.html`
  - `.more-info-dialog` CSS block + `.glance-grid`, `.button-row`, `.entity-row`, `.mi-*` added to `_css.html`

### Wave 3 — cards (all parallel, need scaffold merged ✅)

- [x] **`feat/card-tile`** — inline in `fragments/sensors.html` tile grid (light, switch, fan, cover, lock, binary_sensor)
- [x] **`feat/card-entities`** — entity row CSS + select rendering in area cards
- [x] **`feat/card-glance-sensor`** — glance grid in area cards for sensor entities
- [x] **`feat/card-button`** — button row in area cards for button/scene/script entities

### Wave 4 — more-info dialogs (needs Wave 3 merged ✅)

- [x] **`feat/more-info-domains`** — 12 templates covering all 11 domains + default fallback:
  - `more_info/_light.html` — on/off toggle → `light/toggle`
  - `more_info/_switch.html` — on/off toggle → `switch/toggle`
  - `more_info/_fan.html` — on/off toggle → `fan/toggle`
  - `more_info/_cover.html` — open/stop/close → `cover/{open,stop,close}_cover`
  - `more_info/_lock.html` — lock/unlock → `lock/{lock,unlock}`
  - `more_info/_sensor.html` — value + unit + 20-entry history list
  - `more_info/_binary_sensor.html` — state + 20-entry history list
  - `more_info/_button.html` — press → `button/press`
  - `more_info/_scene.html` — activate → `scene/activate`
  - `more_info/_script.html` — run/stop → `script/{trigger,turn_off}`
  - `more_info/_select.html` — display current option (mutation coming)
  - `more_info/_default.html` — fallback: name + state + device_class

### Wave 5 — dashboard autogeneration (needs Wave 3 + Wave 4 merged ✅)

- [x] **`feat/dashboard-autogen`** — area-grouped dashboard in `feat/dashboard-cards`
  - `build_area_cards()` groups entities by `user_area_id` → resolved to area name
  - Named areas sort alphabetically; "Unassigned" sorts last
  - Card content partitioned by entity_type: tiles / glance / button-row / entity-row
  - `id="area-cards"` container polls every 5 s and responds to `refresh` event
  - Service calls via `POST /ui/services/{domain}/{service}` (form-encoded entity_id)
    close dialog and trigger `refresh` on the area-cards container

### Wave 6 — navigation & UX parity (all parallel, need Wave 5 merged ✅)

Merge order: `feat/nav-pages` → `feat/nav-sidebar` + `feat/nav-settings` (can merge in either order after nav-pages).

- [x] **`feat/nav-pages`** — new page handlers + templates + CSS + area registry type change
  - `load_area_names` → `load_areas` returning `Vec<StoredArea>` at all `app_ctx!` call sites
  - New routes + handlers: `/history`, `/logbook`, `/developer-tools`, `/notifications`, `/system`
  - New area CRUD routes: `GET /areas`, `POST /areas`, `POST /areas/{area_id}/delete`
  - New per-area detail route: `GET /areas/{area_id}`, `GET /fragments/area-sensors/{area_id}`
  - New templates: `history.html`, `logbook.html`, `developer_tools.html`, `notifications.html`,
    `system.html`, `areas.html`, `area_detail.html`
  - CSS: `.nav-badge`, `.settings-list`, `.settings-row`, `.settings-row-icon/text/title/sub/chevron`
  - `_icons.html`: added `icon-history`, `icon-hammer`, `icon-cellphone-cog`, `icon-home-account`

- [x] **`feat/nav-sidebar`** — `_base_app.html` sidebar restructured to match HA Core layout
  - Before-spacer: Overview → per-area auto-generated items (using `area.icon` + fallback) → BLE → Logbook → History
  - After-spacer group 1: Developer tools → Settings
  - Inline `<hr>`-style divider
  - After-spacer group 2: Notifications (bell) → Profile (avatar, `user_name` initial)
  - Removes "Switch Server" nav item

- [x] **`feat/nav-settings`** — settings page redesigned to HA-style list layout
  - `settings.html`: HA-style full-width list rows with colored icon circles
    - Devices & services (blue) → `/devices`
    - Areas, labels & zones (orange) → `/areas`
    - Companion app (purple) → `openNativeSettings()` — canonical entry replaces "Switch Server"
    - People (teal) → `/profile`
    - System (grey) → `/system`
  - `profile.html`: "Switch server" danger zone card removed

---

## Infrastructure / housekeeping

- [ ] Move `tokens.json` filename into a `StorageConfig::tokens_path()` method shared between
      `load_persisted` and `save_persisted`
- [ ] Add integration test for token round-trip persistence (issue → restart → validate)
- [ ] Add integration test for `GET /api/config/device_registry/list` with multiple devices
- [ ] `apply_config_log_level()` in `logging.rs` is a no-op — wire up once
      `tracing-subscriber` supports re-initialisation, or consider `reload` layer
- [ ] Assess whether `feat/onboarding-gaps` (PR #9) needs tests for the new webhook arms
      (`get_config`, `get_zones`, `update_location`, `fire_event`)
