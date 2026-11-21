//! Deep research: a brief turned into a sourced deliverable over a budget of
//! turns.
//!
//! Turn 0 drafts a [`contract::Contract`] that the user approves, revises or
//! rejects. Turns 1..N-2 are waves: a planner on a fresh context reads a
//! [`projection`] of the ledger, records leads (at least one a productive
//! tangent) and sets a frontier, which delegates research in parallel. Turn
//! N-1 is an adversarial critique; turn N writes the deliverable section by
//! section from slices of the ledger and verifies its citations.
//!
//! The [`ledger`] is the only state carried from step to step. It is
//! append-only (`migrations/034_research.sql`); what became of an entry is
//! derived from links on read.

pub mod contract;
pub mod driver;
pub mod ledger;
pub mod ops;
pub mod projection;
pub mod prompts;
pub mod report;
pub mod server;
