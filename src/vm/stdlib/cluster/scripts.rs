//! `cls.script*` / `cls.scripts*` family — fully-replicated scripts store.
//!
//! | Word                | Stack (deepest first)                    | Output                |
//! |---------------------|------------------------------------------|-----------------------|
//! | `cls.script.add`    | `metadata(MAP) script(STR)`              | id (STRING)           |
//! | `cls.script.update` | `id(STR) metadata(MAP) script(STR)`      | nodata                |
//! | `cls.script.delete` | `id(STR)`                                | nodata                |
//! | `cls.script.get`    | `id(STR)`                                | `{id, script, meta}`  |
//! | `cls.scripts.list`  | (no inputs)                              | `[{id, metadata}, …]` |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{
    no_args, one_arg, three_args, two_args, value_to_string,
};

fn cls_script_add_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.script.add", |metadata, script| {
        let s = value_to_string(script, "script", "cls.script.add")?;
        api::scripts::script_add(metadata, &s)
    })
}
pub fn stdlib_cls_script_add_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_script_add_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_script_add_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_script_add_base(vm, StackOps::FromWorkBench) }

fn cls_script_update_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    three_args(vm, op, "cls.script.update", |id, metadata, script| {
        let s = value_to_string(script, "script", "cls.script.update")?;
        api::scripts::script_update(id, metadata, &s)
    })
}
pub fn stdlib_cls_script_update_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_script_update_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_script_update_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_script_update_base(vm, StackOps::FromWorkBench) }

fn cls_script_delete_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.script.delete", |id| api::scripts::script_delete(id))
}
pub fn stdlib_cls_script_delete_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_script_delete_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_script_delete_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_script_delete_base(vm, StackOps::FromWorkBench) }

fn cls_script_get_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.script.get", |id| api::scripts::script_get(id))
}
pub fn stdlib_cls_script_get_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_script_get_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_script_get_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_script_get_base(vm, StackOps::FromWorkBench) }

fn cls_scripts_list_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    no_args(vm, op, || api::scripts::scripts_list())
}
pub fn stdlib_cls_scripts_list_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_scripts_list_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_scripts_list_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_scripts_list_base(vm, StackOps::FromWorkBench) }

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    macro_rules! reg {
        ($name:expr, $stack:ident, $wb:ident) => {{
            let _ = vm.vm.register_inline(format!("{}",  $name), $stack)?;
            let _ = vm.vm.register_inline(format!("{}.", $name), $wb)?;
        }};
    }
    reg!("cls.script.add",     stdlib_cls_script_add_stack,     stdlib_cls_script_add_workbench);
    reg!("cls.script.update",  stdlib_cls_script_update_stack,  stdlib_cls_script_update_workbench);
    reg!("cls.script.delete",  stdlib_cls_script_delete_stack,  stdlib_cls_script_delete_workbench);
    reg!("cls.script.get",     stdlib_cls_script_get_stack,     stdlib_cls_script_get_workbench);
    reg!("cls.scripts.list",   stdlib_cls_scripts_list_stack,   stdlib_cls_scripts_list_workbench);
    Ok(())
}
