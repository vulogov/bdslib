//! `cls.signal*` family — emit / update / get / recent / query.
//!
//! | Word                  | Stack (deepest first)                              | Output                  |
//! |-----------------------|----------------------------------------------------|-------------------------|
//! | `cls.signal.emit`     | `name(STR) severity(STR) ts(INT) extra(MAP)`       | id (STRING)             |
//! | `cls.signal.update`   | `id(STR) metadata(MAP)`                            | nodata                  |
//! | `cls.signal.get`      | `id(STR)`                                          | metadata Map (or null)  |
//! | `cls.signals.recent`  | `duration(STR)`                                    | `{count, signals: […]}` |
//! | `cls.signals.query`   | `query(STR) limit(INT)`                            | `{count, results: […]}` |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{
    four_args, one_arg, two_args, value_to_string, value_to_u64, value_to_usize,
};

fn cls_signal_emit_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    four_args(vm, op, "cls.signal.emit", |name, severity, ts, extra| {
        let n  = value_to_string(name,     "name",      "cls.signal.emit")?;
        let sv = value_to_string(severity, "severity",  "cls.signal.emit")?;
        let t  = value_to_u64(ts,          "timestamp", "cls.signal.emit")?;
        api::signals::signal_emit(&n, &sv, t, extra)
    })
}
pub fn stdlib_cls_signal_emit_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_signal_emit_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_signal_emit_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_signal_emit_base(vm, StackOps::FromWorkBench) }

fn cls_signal_update_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.signal.update", |id, metadata| api::signals::signal_update(id, metadata))
}
pub fn stdlib_cls_signal_update_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_signal_update_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_signal_update_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_signal_update_base(vm, StackOps::FromWorkBench) }

fn cls_signal_get_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.signal.get", |id| api::signals::signal_get(id))
}
pub fn stdlib_cls_signal_get_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_signal_get_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_signal_get_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_signal_get_base(vm, StackOps::FromWorkBench) }

fn cls_signals_recent_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.signals.recent", |dur| {
        let d = value_to_string(dur, "duration", "cls.signals.recent")?;
        api::signals::signals_recent(&d)
    })
}
pub fn stdlib_cls_signals_recent_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_signals_recent_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_signals_recent_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_signals_recent_base(vm, StackOps::FromWorkBench) }

fn cls_signals_query_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.signals.query", |query, limit| {
        let q   = value_to_string(query, "query", "cls.signals.query")?;
        let lim = value_to_usize(limit,  "limit", "cls.signals.query")?;
        api::signals::signals_query(&q, lim)
    })
}
pub fn stdlib_cls_signals_query_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_signals_query_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_signals_query_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_signals_query_base(vm, StackOps::FromWorkBench) }

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    macro_rules! reg {
        ($name:expr, $stack:ident, $wb:ident) => {{
            let _ = vm.vm.register_inline(format!("{}",  $name), $stack)?;
            let _ = vm.vm.register_inline(format!("{}.", $name), $wb)?;
        }};
    }
    reg!("cls.signal.emit",    stdlib_cls_signal_emit_stack,    stdlib_cls_signal_emit_workbench);
    reg!("cls.signal.update",  stdlib_cls_signal_update_stack,  stdlib_cls_signal_update_workbench);
    reg!("cls.signal.get",     stdlib_cls_signal_get_stack,     stdlib_cls_signal_get_workbench);
    reg!("cls.signals.recent", stdlib_cls_signals_recent_stack, stdlib_cls_signals_recent_workbench);
    reg!("cls.signals.query",  stdlib_cls_signals_query_stack,  stdlib_cls_signals_query_workbench);
    Ok(())
}
