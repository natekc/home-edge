//! Canonical Home Assistant protocol types for protocol-level compatibility.
//!
//! Each type in this module is derived from the Home Assistant Python source
//! and must match the exact field names, value formats, and serialisation
//! behaviour that real HA clients (mobile app, web UI, third-party REST/WS
//! clients) depend on.
//!
//! Source-of-truth references:
//!   - homeassistant/core.py        – State, Context, CoreState, Event
//!   - homeassistant/const.py       – URL_API_* constants, CoreState values
//!   - homeassistant/components/api/__init__.py – REST response shapes
//!   - homeassistant/components/websocket_api/  – WS command/response shapes

pub mod api;
pub mod context;
pub mod core_state;
pub mod entity;
pub mod event;
