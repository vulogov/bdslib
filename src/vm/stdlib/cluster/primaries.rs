//! `cls.primary*` / `cls.secondary*` / `cls.primaries*` family.
//!
//! | Word                              | Stack (deepest first)        | Output                |
//! |-----------------------------------|------------------------------|-----------------------|
//! | `cls.primaries`                   | `opts(MAP)`                  | `{ids: [STR, …]}`     |
//! | `cls.primaries.explore`           | `duration(STR)`              | `{results: […]}`      |
//! | `cls.primaries.explore.telemetry` | `duration(STR)`              | `{results: […]}`      |
//! | `cls.primaries.get`               | `duration(STR) key(STR)`     | `{results: […]}`      |
//! | `cls.primaries.get.telemetry`     | `duration(STR) key(STR)`     | `{results: […]}`      |
//! | `cls.secondaries`                 | `primary_id(STR)`            | `{ids: [STR, …]}`     |
//! | `cls.primary`                     | `id(STR)`                    | record Map            |
//! | `cls.secondary`                   | `id(STR)`                    | record Map            |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{one_arg, two_args, value_to_string};

fn cls_primaries_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.primaries", |opts| api::primaries::primaries(opts))
}
pub fn stdlib_cls_primaries_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_primaries_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_primaries_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_primaries_base(vm, StackOps::FromWorkBench) }

fn cls_primaries_explore_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.primaries.explore", |dur| {
        let d = value_to_string(dur, "duration", "cls.primaries.explore")?;
        api::primaries::primaries_explore(&d)
    })
}
pub fn stdlib_cls_primaries_explore_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_primaries_explore_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_primaries_explore_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_primaries_explore_base(vm, StackOps::FromWorkBench) }

fn cls_primaries_explore_telemetry_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.primaries.explore.telemetry", |dur| {
        let d = value_to_string(dur, "duration", "cls.primaries.explore.telemetry")?;
        api::primaries::primaries_explore_telemetry(&d)
    })
}
pub fn stdlib_cls_primaries_explore_telemetry_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_primaries_explore_telemetry_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_primaries_explore_telemetry_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_primaries_explore_telemetry_base(vm, StackOps::FromWorkBench) }

fn cls_primaries_get_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.primaries.get", |duration, key| {
        let dur = value_to_string(duration, "duration", "cls.primaries.get")?;
        let k   = value_to_string(key,      "key",      "cls.primaries.get")?;
        api::primaries::primaries_get(&dur, &k)
    })
}
pub fn stdlib_cls_primaries_get_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_primaries_get_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_primaries_get_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_primaries_get_base(vm, StackOps::FromWorkBench) }

fn cls_primaries_get_telemetry_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.primaries.get.telemetry", |duration, key| {
        let dur = value_to_string(duration, "duration", "cls.primaries.get.telemetry")?;
        let k   = value_to_string(key,      "key",      "cls.primaries.get.telemetry")?;
        api::primaries::primaries_get_telemetry(&dur, &k)
    })
}
pub fn stdlib_cls_primaries_get_telemetry_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_primaries_get_telemetry_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_primaries_get_telemetry_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_primaries_get_telemetry_base(vm, StackOps::FromWorkBench) }

fn cls_secondaries_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.secondaries", |id| api::primaries::secondaries(id))
}
pub fn stdlib_cls_secondaries_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_secondaries_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_secondaries_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_secondaries_base(vm, StackOps::FromWorkBench) }

fn cls_primary_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.primary", |id| api::primaries::primary(id))
}
pub fn stdlib_cls_primary_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_primary_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_primary_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_primary_base(vm, StackOps::FromWorkBench) }

fn cls_secondary_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.secondary", |id| api::primaries::secondary(id))
}
pub fn stdlib_cls_secondary_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_secondary_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_secondary_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_secondary_base(vm, StackOps::FromWorkBench) }

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    macro_rules! reg {
        ($name:expr, $stack:ident, $wb:ident) => {{
            let _ = vm.vm.register_inline(format!("{}",  $name), $stack)?;
            let _ = vm.vm.register_inline(format!("{}.", $name), $wb)?;
        }};
    }
    reg!("cls.primaries",                     stdlib_cls_primaries_stack,                     stdlib_cls_primaries_workbench);
    reg!("cls.primaries.explore",             stdlib_cls_primaries_explore_stack,             stdlib_cls_primaries_explore_workbench);
    reg!("cls.primaries.explore.telemetry",   stdlib_cls_primaries_explore_telemetry_stack,   stdlib_cls_primaries_explore_telemetry_workbench);
    reg!("cls.primaries.get",                 stdlib_cls_primaries_get_stack,                 stdlib_cls_primaries_get_workbench);
    reg!("cls.primaries.get.telemetry",       stdlib_cls_primaries_get_telemetry_stack,       stdlib_cls_primaries_get_telemetry_workbench);
    reg!("cls.secondaries",                   stdlib_cls_secondaries_stack,                   stdlib_cls_secondaries_workbench);
    reg!("cls.primary",                       stdlib_cls_primary_stack,                       stdlib_cls_primary_workbench);
    reg!("cls.secondary",                     stdlib_cls_secondary_stack,                     stdlib_cls_secondary_workbench);
    Ok(())
}
