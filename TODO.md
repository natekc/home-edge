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

- [ ] **`feat/card-templates-scaffold`** — prerequisite for all card branches
  - Create `crates/controller/templates/cards/` directory
  - Create `crates/controller/templates/rows/` directory
  - Create `crates/controller/templates/more_info/` directory
  - Add `<dialog id="more-info-dialog">` shell to `_base_app.html` (before `</body>`)
  - Add `.more-info-dialog` CSS block to `_css.html`

### Wave 3 — cards (all parallel, need scaffold merged)

- [ ] **`feat/card-tile`** — generic tile card (icon + state + tap action)
- [ ] **`feat/card-entities`** — entities list card (rows of icon + name + state)
- [ ] **`feat/card-glance-sensor`** — glance/sensor card (compact multi-value grid)
- [ ] **`feat/card-button`** — button card (one-tap service call)

### Wave 4 — more-info dialogs (needs Wave 3 merged)

- [ ] **`feat/more-info-domains`** — per-domain detail dialogs triggered from cards
  - Domains: `light`, `switch`, `cover`, `lock`, `fan`, `sensor`,
    `binary_sensor`, `button`, `scene`, `script`, `select`

### Wave 5 — dashboard autogeneration (needs Wave 3 + Wave 4 merged)

- [ ] **`feat/dashboard-autogen`** — generate a default dashboard from entity registry
  - Group entities by area, assign card type by domain
  - Render via the card templates added in Wave 3

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
