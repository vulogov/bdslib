//! Shared time-window parsing for the `vm::api::*` helpers.
//!
//! Three input shapes are accepted (mirroring `params::TimeWindowParams`
//! in the bdsnode JSON-RPC layer):
//!
//! - empty / null → [`Window::All`]
//! - `{"duration": "1h"}` → [`Window::Range`] derived from `now`
//! - `{"start_ts": 1700000000, "end_ts": 1700001000}` → explicit range
//!
//! Helpers use this so a Bund script can pass a Map with the same
//! field names the v2 JSON-RPC methods accept and have it Just Work.

use crate::shardsinfo::{ShardInfo, ShardInfoEngine};
use crate::common::error::Result as InternalResult;
use easy_error::{err_msg, Error};
use serde_json::Value as JsonValue;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Resolved time window — either "every shard ever" or an explicit
/// `[start, end)` range.
#[derive(Debug, Clone, Copy)]
pub enum Window {
    All,
    Range(SystemTime, SystemTime),
}

impl Window {
    /// Convenience: feed `info.list_all()` or `info.shards_in_range()`
    /// from the same call site without duplicated branching.
    pub fn list_shards(&self, info: &ShardInfoEngine) -> InternalResult<Vec<ShardInfo>> {
        match self {
            Window::All                => info.list_all(),
            Window::Range(start, end)  => info.shards_in_range(*start, *end),
        }
    }
}

/// Parse the conventional time-window fields out of a JSON object.
/// `null` / non-object inputs default to [`Window::All`].
pub fn resolve_window(opts: &JsonValue) -> Result<Window, Error> {
    let obj = match opts.as_object() {
        Some(o) if !o.is_empty() => o,
        _                        => return Ok(Window::All),
    };
    if let Some(d) = obj.get("duration").and_then(|v| v.as_str()) {
        let secs = humantime::parse_duration(d)
            .map_err(|e| err_msg(format!("invalid duration {d:?}: {e}")))?
            .as_secs();
        let end   = SystemTime::now();
        let start = end - Duration::from_secs(secs);
        return Ok(Window::Range(start, end));
    }
    if let (Some(s), Some(e)) = (
        obj.get("start_ts").and_then(|v| v.as_i64()),
        obj.get("end_ts").and_then(|v| v.as_i64()),
    ) {
        let start = UNIX_EPOCH + Duration::from_secs(s as u64);
        let end   = UNIX_EPOCH + Duration::from_secs(e as u64);
        return Ok(Window::Range(start, end));
    }
    Ok(Window::All)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_object_is_all() {
        assert!(matches!(resolve_window(&json!({})).unwrap(), Window::All));
    }

    #[test]
    fn null_is_all() {
        assert!(matches!(resolve_window(&JsonValue::Null).unwrap(), Window::All));
    }

    #[test]
    fn duration_parses_to_range() {
        let w = resolve_window(&json!({"duration": "30s"})).unwrap();
        assert!(matches!(w, Window::Range(_, _)));
    }

    #[test]
    fn explicit_ts_pair_parses_to_range() {
        let w = resolve_window(&json!({"start_ts": 100, "end_ts": 200})).unwrap();
        match w {
            Window::Range(s, e) => {
                let s = s.duration_since(UNIX_EPOCH).unwrap().as_secs();
                let e = e.duration_since(UNIX_EPOCH).unwrap().as_secs();
                assert_eq!(s, 100);
                assert_eq!(e, 200);
            }
            _ => panic!("expected Range"),
        }
    }

    #[test]
    fn invalid_duration_errors() {
        assert!(resolve_window(&json!({"duration": "🤖"})).is_err());
    }
}
