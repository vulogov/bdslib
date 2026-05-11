//! `cls.keys*` family — telemetry-key enumeration.
//!
//! | Word            | Stack (deepest first)                | Output             |
//! |-----------------|--------------------------------------|--------------------|
//! | `cls.keys`      | `duration(STR)`                      | `{keys: [STR, …]}` |
//! | `cls.keys.all`  | `duration(STR) pattern(STR)`         | `{keys: [STR, …]}` |
//! | `cls.keys.get`  | `duration(STR) key(STR)`             | `{results: […]}`   |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{one_arg, two_args, value_to_string};

fn cls_keys_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.keys", |dur| {
        let d = value_to_string(dur, "duration", "cls.keys")?;
        api::keys::keys(&d)
    })
}
pub fn stdlib_cls_keys_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_keys_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_keys_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_keys_base(vm, StackOps::FromWorkBench) }

fn cls_keys_all_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.keys.all", |duration, pattern| {
        let dur = value_to_string(duration, "duration", "cls.keys.all")?;
        let pat = value_to_string(pattern,  "pattern",  "cls.keys.all")?;
        api::keys::keys_all(&dur, &pat)
    })
}
pub fn stdlib_cls_keys_all_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_keys_all_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_keys_all_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_keys_all_base(vm, StackOps::FromWorkBench) }

fn cls_keys_get_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.keys.get", |duration, key| {
        let dur = value_to_string(duration, "duration", "cls.keys.get")?;
        let k   = value_to_string(key,      "key",      "cls.keys.get")?;
        api::keys::keys_get(&dur, &k)
    })
}
pub fn stdlib_cls_keys_get_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_keys_get_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_keys_get_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_keys_get_base(vm, StackOps::FromWorkBench) }

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    let _ = vm.vm.register_inline("cls.keys".into(),       stdlib_cls_keys_stack)?;
    let _ = vm.vm.register_inline("cls.keys.".into(),      stdlib_cls_keys_workbench)?;
    let _ = vm.vm.register_inline("cls.keys.all".into(),   stdlib_cls_keys_all_stack)?;
    let _ = vm.vm.register_inline("cls.keys.all.".into(),  stdlib_cls_keys_all_workbench)?;
    let _ = vm.vm.register_inline("cls.keys.get".into(),   stdlib_cls_keys_get_stack)?;
    let _ = vm.vm.register_inline("cls.keys.get.".into(),  stdlib_cls_keys_get_workbench)?;
    Ok(())
}
