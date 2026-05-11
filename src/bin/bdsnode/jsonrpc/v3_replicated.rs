//! Shared fan-out helper for v3 fully-replicated stores (docs, signals,
//! scripts).
//!
//! Re-export of `bdslib::cluster::replication::{replicate_to_all,
//! Outcome}` — the same code is used by `vm::api::*` Bund helpers when
//! a script writes under cluster mode, so it lives in the library.

#[allow(unused_imports)]
pub use bdslib::cluster::replication::{replicate_to_all, Outcome};
