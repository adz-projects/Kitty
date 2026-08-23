//! Vector math backing recall: DPP selection, activation spreading, and
//! the small ops shared by both.
//!
//! `index` (a brute-force `VectorIndex`) and `cms` (a count-min sketch)
//! used to live here too. Neither had a single caller outside its own unit
//! tests — recall scans `list_recall_candidates`' bounded row set directly
//! — so they were carried into every build of the daemon for nothing.

pub mod dpp;
pub mod ops;
pub mod spread;
