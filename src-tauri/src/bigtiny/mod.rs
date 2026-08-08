//! BigTiny REST/SSE client — Kitty's only chat backend. All BigTiny endpoint
//! paths / transport details live here; the session commands in
//! `crate::commands::session` delegate straight to it, and this module emits
//! the exact `chat://*` / `session://*` Tauri events the React frontend
//! consumes.
//!
//! BigTiny protocol (see BigTiny's API.md): plain REST over localhost with an
//! `X-API-Key` header, plus one streaming endpoint — `POST
//! /api/chat/{id}/send` returns `data: {json}\n\n` SSE frames (`SSEEvent`
//! objects) ending at `is_last: true`.

pub mod client;
pub mod mcp;
pub mod pathway;
pub mod providers;
pub mod sessions;
pub mod stream;
