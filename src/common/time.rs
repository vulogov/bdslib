use std::time::{SystemTime, UNIX_EPOCH};

/// Return the current time as whole Unix seconds (infallible).
///
/// Unlike [`crate::common::timerange::now_unix_secs`], this function never
/// returns an error: it saturates to `0` if the system clock predates the Unix
/// epoch, which is not possible on any supported OS.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
