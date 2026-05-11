//! Shared stack/workbench plumbing for the `cls.*` words.
//!
//! Re-exports the `db::doc_helpers` primitives plus a few cluster-
//! specific extras (binary-bytes extractor for content payloads,
//! u64-from-int helper for the `start_ts`/`end_ts`/`timestamp` words).

extern crate log;

use easy_error::{bail, Error};
use rust_dynamic::types::Val;
use rust_dynamic::value::Value;
use rust_multistackvm::multistackvm::{StackOps, VM};

pub use crate::vm::stdlib::db::doc_helpers::{
    pull, push, require_depth, value_to_f32_vec, value_to_string, value_to_uuid,
};

/// Convert a `Value` to a byte buffer.
///
/// - `Val::Binary(b)` → bytes verbatim
/// - `Val::String(s)` → UTF-8 bytes
/// - `Val::List(items)` → expects each item to be an integer in 0..=255
pub fn value_to_bytes(v: Value, err_prefix: &str) -> Result<Vec<u8>, Error> {
    match v.data {
        Val::Binary(b) => Ok(b),
        Val::String(s) => Ok(s.into_bytes()),
        Val::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.cast_int() {
                    Ok(i) if (0..=255).contains(&i) => out.push(i as u8),
                    Ok(i)    => bail!("{} byte out of 0..=255: {}", err_prefix, i),
                    Err(err) => bail!("{} byte cast failed: {}", err_prefix, err),
                }
            }
            Ok(out)
        }
        other => bail!("{} expected Binary/String/List, got {:?}", err_prefix, other),
    }
}

/// Cast a `Value` to `u64`.  Negative integers error.
pub fn value_to_u64(v: Value, field: &str, err_prefix: &str) -> Result<u64, Error> {
    let i = v.cast_int()
        .map_err(|e| easy_error::err_msg(format!("{err_prefix} {field} cast failed: {e}")))?;
    if i < 0 {
        bail!("{} {} must be ≥ 0, got {}", err_prefix, field, i);
    }
    Ok(i as u64)
}

/// Cast a `Value` to `usize`.  Negative integers error.
pub fn value_to_usize(v: Value, field: &str, err_prefix: &str) -> Result<usize, Error> {
    Ok(value_to_u64(v, field, err_prefix)? as usize)
}

/// Helper for the very common pattern: depth-check, pull N values from
/// the chosen source, hand them to `body` in **stack order** (i.e.
/// `args[0]` is the deepest, `args[N-1]` is TOS).  `body` returns the
/// final `Value` to push back.
pub fn one_arg<'a, F>(
    vm:         &'a mut VM,
    op:         StackOps,
    err_prefix: &str,
    body:       F,
) -> Result<&'a mut VM, Error>
where
    F: FnOnce(Value) -> Result<Value, Error>,
{
    require_depth(vm, &op, 1, err_prefix)?;
    let v = pull(vm, &op).unwrap();
    let result = body(v)?;
    push(vm, &op, result);
    Ok(vm)
}

pub fn two_args<'a, F>(
    vm:         &'a mut VM,
    op:         StackOps,
    err_prefix: &str,
    body:       F,
) -> Result<&'a mut VM, Error>
where
    F: FnOnce(Value, Value) -> Result<Value, Error>,
{
    require_depth(vm, &op, 2, err_prefix)?;
    let b = pull(vm, &op).unwrap();
    let a = pull(vm, &op).unwrap();
    let result = body(a, b)?;
    push(vm, &op, result);
    Ok(vm)
}

pub fn three_args<'a, F>(
    vm:         &'a mut VM,
    op:         StackOps,
    err_prefix: &str,
    body:       F,
) -> Result<&'a mut VM, Error>
where
    F: FnOnce(Value, Value, Value) -> Result<Value, Error>,
{
    require_depth(vm, &op, 3, err_prefix)?;
    let c = pull(vm, &op).unwrap();
    let b = pull(vm, &op).unwrap();
    let a = pull(vm, &op).unwrap();
    let result = body(a, b, c)?;
    push(vm, &op, result);
    Ok(vm)
}

pub fn four_args<'a, F>(
    vm:         &'a mut VM,
    op:         StackOps,
    err_prefix: &str,
    body:       F,
) -> Result<&'a mut VM, Error>
where
    F: FnOnce(Value, Value, Value, Value) -> Result<Value, Error>,
{
    require_depth(vm, &op, 4, err_prefix)?;
    let d = pull(vm, &op).unwrap();
    let c = pull(vm, &op).unwrap();
    let b = pull(vm, &op).unwrap();
    let a = pull(vm, &op).unwrap();
    let result = body(a, b, c, d)?;
    push(vm, &op, result);
    Ok(vm)
}

/// Word with no inputs and one output (e.g. `?cluster.meta`,
/// `cls.timeline`, `cls.scripts.list`).
pub fn no_args<'a, F>(
    vm:   &'a mut VM,
    op:   StackOps,
    body: F,
) -> Result<&'a mut VM, Error>
where
    F: FnOnce() -> Result<Value, Error>,
{
    let result = body()?;
    push(vm, &op, result);
    Ok(vm)
}
