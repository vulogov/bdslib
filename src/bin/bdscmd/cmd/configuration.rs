//! `bdscmd configuration` — dump the target bdsnode's effective
//! configuration via `v2/configuration`.
//!
//! Returns the parsed `bds.hjson` (the keys the operator actually
//! wrote) alongside a library-defaults block (what bdslib falls back
//! to for keys the operator omitted), plus the resolved config path
//! and load time.  Useful for:
//!
//! - Verifying which file a remote node is actually running with when
//!   `--config` / `BDS_CONFIG` may have been overridden in a wrapper.
//! - Diffing config across cluster peers (`bdscmd --url http://nodeN
//!   configuration | jq '.config'`).
//! - Feeding into a downstream review (cf. the same response powers
//!   the bdsweb Administration → Configuration page and its
//!   "Analyze this!" LLM review).
//!
//! No arguments — single read.  Unauthenticated v2/* (same trust
//! boundary as `v2/status`).

use anyhow::Result;
use clap::Args;
use serde_json::{Map, Value};

#[derive(Args)]
pub struct Cmd;

pub fn run(url: &str, _session: &str, _args: Cmd) -> Result<Value> {
    crate::client::call(url, "v2/configuration", Value::Object(Map::new()))
}
