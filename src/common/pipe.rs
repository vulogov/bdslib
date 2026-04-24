use crate::common::error::{err_msg, Result};
use crossbeam::channel::{unbounded, Receiver, RecvTimeoutError, Sender, TryRecvError};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

/// A named unbounded MPMC channel pair.
pub struct Pipe {
    pub sender: Sender<Value>,
    pub receiver: Receiver<Value>,
}

static REGISTRY: OnceLock<HashMap<String, Pipe>> = OnceLock::new();

/// Create one unbounded channel per name and register them globally.
///
/// Must be called exactly once before any [`send`] / [`recv`] call.
/// Returns `Err` if called a second time.
pub fn init(names: &[&str]) -> Result<()> {
    let map: HashMap<String, Pipe> = names
        .iter()
        .map(|&name| {
            let (sender, receiver) = unbounded();
            (name.to_string(), Pipe { sender, receiver })
        })
        .collect();
    REGISTRY
        .set(map)
        .map_err(|_| err_msg("pipe registry already initialized"))
}

fn get(name: &str) -> Result<&'static Pipe> {
    REGISTRY
        .get()
        .ok_or_else(|| err_msg("pipe registry not initialized; call pipe::init() first"))?
        .get(name)
        .ok_or_else(|| err_msg(format!("pipe {name:?} not found in registry")))
}

/// Send `value` to the named channel. Never blocks (unbounded).
pub fn send(name: &str, value: Value) -> Result<()> {
    get(name)?
        .sender
        .send(value)
        .map_err(|e| err_msg(e.to_string()))
}

/// Block until a value is available on the named channel.
pub fn recv(name: &str) -> Result<Value> {
    get(name)?
        .receiver
        .recv()
        .map_err(|e| err_msg(e.to_string()))
}

/// Non-blocking receive. Returns `Ok(None)` when the channel is empty.
pub fn try_recv(name: &str) -> Result<Option<Value>> {
    match get(name)?.receiver.try_recv() {
        Ok(v) => Ok(Some(v)),
        Err(TryRecvError::Empty) => Ok(None),
        Err(e) => Err(err_msg(e.to_string())),
    }
}

/// Block for at most `timeout`. Returns `Ok(None)` on timeout.
pub fn recv_timeout(name: &str, timeout: Duration) -> Result<Option<Value>> {
    match get(name)?.receiver.recv_timeout(timeout) {
        Ok(v) => Ok(Some(v)),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(e) => Err(err_msg(e.to_string())),
    }
}

/// Borrow the raw [`Receiver`] for `name`.
///
/// Intended for callers that need to use `crossbeam::select!` across
/// multiple channels simultaneously (e.g. combining data and shutdown signals).
pub fn receiver(name: &str) -> Result<&'static Receiver<Value>> {
    Ok(&get(name)?.receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;
    use std::thread;

    // The global registry is a OnceLock, so init() must be called exactly once
    // across all tests in the process. We pre-register every channel name used
    // by any test here, guarded by a Once.
    static INIT: Once = Once::new();

    fn setup() {
        INIT.call_once(|| {
            init(&[
                "t_basic",
                "t_empty",
                "t_value",
                "t_timeout_empty",
                "t_timeout_val",
                "t_order",
                "t_mpmc",
            ])
            .expect("pipe init failed");
        });
    }

    #[test]
    fn test_send_recv_roundtrip() {
        setup();
        let v = serde_json::json!({"key": "hello", "n": 42});
        send("t_basic", v.clone()).unwrap();
        assert_eq!(recv("t_basic").unwrap(), v);
    }

    #[test]
    fn test_try_recv_empty_channel() {
        setup();
        assert_eq!(try_recv("t_empty").unwrap(), None);
    }

    #[test]
    fn test_try_recv_with_value() {
        setup();
        let v = serde_json::json!("world");
        send("t_value", v.clone()).unwrap();
        assert_eq!(try_recv("t_value").unwrap(), Some(v));
        // channel is now drained
        assert_eq!(try_recv("t_value").unwrap(), None);
    }

    #[test]
    fn test_recv_timeout_expires() {
        setup();
        let result = recv_timeout("t_timeout_empty", Duration::from_millis(30)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_recv_timeout_receives_before_deadline() {
        setup();
        let v = serde_json::json!(99);
        send("t_timeout_val", v.clone()).unwrap();
        let result = recv_timeout("t_timeout_val", Duration::from_millis(100)).unwrap();
        assert_eq!(result, Some(v));
    }

    #[test]
    fn test_fifo_ordering() {
        setup();
        for i in 0..8u64 {
            send("t_order", serde_json::json!(i)).unwrap();
        }
        for i in 0..8u64 {
            assert_eq!(recv("t_order").unwrap(), serde_json::json!(i));
        }
    }

    #[test]
    fn test_mpmc_multiple_producers() {
        setup();
        let handles: Vec<_> = (0..4)
            .map(|i| {
                thread::spawn(move || {
                    send("t_mpmc", serde_json::json!(i)).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let mut received: Vec<u64> = (0..4)
            .map(|_| recv("t_mpmc").unwrap().as_u64().unwrap())
            .collect();
        received.sort_unstable();
        assert_eq!(received, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_unknown_channel_send_errors() {
        setup();
        assert!(send("no_such_channel", serde_json::json!(1)).is_err());
    }

    #[test]
    fn test_unknown_channel_recv_errors() {
        setup();
        assert!(recv_timeout("no_such_channel", Duration::from_millis(1)).is_err());
    }

    #[test]
    fn test_init_twice_returns_error() {
        setup(); // first init already happened via Once
        assert!(init(&["duplicate"]).is_err());
    }
}
