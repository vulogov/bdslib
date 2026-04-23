use jsonrpsee::types::ErrorObject;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn rpc_err(code: i32, msg: impl std::fmt::Display) -> ErrorObject<'static> {
    ErrorObject::owned(code, msg.to_string(), None::<()>)
}

/// Optional time-window parameters accepted by several methods.
#[derive(serde::Deserialize, Default)]
pub struct TimeWindowParams {
    /// Lookback window, e.g. `"1h"`, `"30min"`.
    pub duration: Option<String>,
    /// Range start as Unix seconds. Requires `end_ts`.
    pub start_ts: Option<i64>,
    /// Range end as Unix seconds. Requires `start_ts`.
    pub end_ts: Option<i64>,
}

pub enum TimeWindow {
    All,
    Range(SystemTime, SystemTime),
}

impl TimeWindowParams {
    pub fn resolve(self) -> Result<TimeWindow, ErrorObject<'static>> {
        if let Some(ref d) = self.duration {
            let secs = humantime::parse_duration(d)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {d:?}: {e}")))?
                .as_secs();
            let end = SystemTime::now();
            let start = end - Duration::from_secs(secs);
            Ok(TimeWindow::Range(start, end))
        } else if let (Some(s), Some(e)) = (self.start_ts, self.end_ts) {
            let start = UNIX_EPOCH + Duration::from_secs(s as u64);
            let end = UNIX_EPOCH + Duration::from_secs(e as u64);
            Ok(TimeWindow::Range(start, end))
        } else {
            Ok(TimeWindow::All)
        }
    }
}
