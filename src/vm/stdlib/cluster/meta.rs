//! `?cluster.meta` — read the per-thread cluster meta from the
//! most-recent `cls.*` call.
//!
//! Pushes a `Value::Map` (the JSON object emitted by the dispatch
//! layer with shape
//! `{enabled, peers_queried, peers_answered, partial, failed}`
//! for reads or `{enabled, replication: {peers_attempted, …}}` for
//! writes), or `Value::nodata()` if no cluster-aware helper has run
//! on this thread yet.
//!
//! `?cluster.meta.` is the workbench-targeted twin.

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_dynamic::value::Value;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::helpers::eval::json_to_dynamic;
use crate::vm::stdlib::cluster::helpers::push;

fn meta_value() -> Value {
    match api::meta::get() {
        Some(m) => json_to_dynamic(m),
        None    => Value::nodata(),
    }
}

pub fn stdlib_cluster_meta_stack(vm: &mut VM) -> Result<&mut VM, Error> {
    push(vm, &StackOps::FromStack, meta_value());
    Ok(vm)
}

pub fn stdlib_cluster_meta_workbench(vm: &mut VM) -> Result<&mut VM, Error> {
    push(vm, &StackOps::FromWorkBench, meta_value());
    Ok(vm)
}

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    let _ = vm.vm.register_inline("?cluster.meta".to_string(),  stdlib_cluster_meta_stack)?;
    let _ = vm.vm.register_inline("?cluster.meta.".to_string(), stdlib_cluster_meta_workbench)?;
    Ok(())
}
