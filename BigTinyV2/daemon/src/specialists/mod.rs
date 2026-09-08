//! Specialists: named delegate agents the main model can call mid-turn.
//!
//! Three pieces, deliberately separated:
//!
//! * [`registry`] — what the built-in specialists *are*, and the rules any
//!   definition must satisfy. Pure data plus validation.
//! * [`server`] — the in-process MCP server that puts `call_specialist` in
//!   front of the model.
//! * `agent::orchestrator` — how a delegate is actually run, safely. It knows
//!   nothing about specialists, only about parent/child turns, which is what
//!   lets this definition format change without touching it.

pub mod registry;
pub mod server;
