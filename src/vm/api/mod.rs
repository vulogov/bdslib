//! Cluster-aware Bund VM helpers.
//!
//! `vm::api::*` exposes one function per logical operation
//! (add / search / analysis / signals / documents / …) with a uniform
//! contract:
//!
//! - **Inputs and outputs are `rust_dynamic::value::Value`.**  Conversion
//!   to/from `serde_json::Value` happens inside the helper via
//!   `vm::helpers::eval::{dynamic_to_json, json_to_dynamic}`.  Bund
//!   stdlib word handlers stay free of JSON details.
//!
//! - **Standalone vs cluster mode is detected automatically** via
//!   `bdslib::get_db()?.cluster()`.  When cluster mode is on, the
//!   helper transparently fans out the matching v2/* method to every
//!   Alive peer and merges the responses with the same code the
//!   bdsnode `v3_*` handlers use (`crate::cluster::merge`).  When
//!   standalone, the helper simply runs the local DB call.
//!
//! - **The same global DB handle** is used for both paths — there is
//!   no duplicate "local API" vs "cluster API".
//!
//! - **Per-call cluster meta** (peers_queried/answered/partial/failed)
//!   is stashed on a per-thread cell and exposed to Bund scripts via
//!   the `?cluster.meta` word.  Helpers themselves return only the
//!   merged data, keeping their shape identical to a local call.
//!
//! Phase 2 ships only the foundation modules:
//!
//! | Module      | Role                                                    |
//! |-------------|---------------------------------------------------------|
//! | `runtime`   | `block_on(future)` — sync→async bridge                  |
//! | `meta`      | thread-local `LAST_META` for `?cluster.meta`            |
//! | `dispatch`  | `read(...)` — generic local-vs-cluster recipe           |
//!
//! Per-area modules (add/search/analysis/signals/documents/…) land in
//! Phase 3.

pub mod dispatch;
pub mod meta;
pub mod runtime;
pub mod time_window;

// Area modules
pub mod add;
pub mod analysis;
pub mod documents;
pub mod keys;
pub mod llm;
pub mod primaries;
pub mod scripts;
pub mod search;
pub mod signals;
pub mod templates;
