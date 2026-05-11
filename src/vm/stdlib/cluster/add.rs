//! `cls.add` family — observability ingest, count, duplicates,
//! fingerprints.
//!
//! | Word                      | Stack (deepest first)         | Result                  |
//! |---------------------------|--------------------------------|-------------------------|
//! | `cls.add`                 | `doc(MAP)`                     | id (STRING)             |
//! | `cls.add.batch`           | `docs(LIST<MAP>)`              | ids (LIST<STRING>)      |
//! | `cls.update`              | `id(STR) doc(MAP)`             | new_id (STRING)         |
//! | `cls.delete`              | `id(STR)`                      | nodata                  |
//! | `cls.count`               | `opts(MAP)`                    | `{count, local_count}`  |
//! | `cls.duplicates`          | `opts(MAP)`                    | `{id: [ts, …]}` map     |
//! | `cls.fingerprints.recent` | `duration(STR)`                | `{fingerprints: […]}`   |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{
    one_arg, two_args, value_to_string,
};

// ── cls.add ──────────────────────────────────────────────────────────────────

fn cls_add_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.add", |doc| api::add::add(doc))
}
pub fn stdlib_cls_add_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_add_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_add_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_add_base(vm, StackOps::FromWorkBench) }

// ── cls.add.batch ────────────────────────────────────────────────────────────

fn cls_add_batch_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.add.batch", |docs| api::add::add_batch(docs))
}
pub fn stdlib_cls_add_batch_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_add_batch_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_add_batch_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_add_batch_base(vm, StackOps::FromWorkBench) }

// ── cls.update ───────────────────────────────────────────────────────────────

fn cls_update_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.update", |id, doc| api::add::update(id, doc))
}
pub fn stdlib_cls_update_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_update_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_update_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_update_base(vm, StackOps::FromWorkBench) }

// ── cls.delete ───────────────────────────────────────────────────────────────

fn cls_delete_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.delete", |id| api::add::delete_by_id(id))
}
pub fn stdlib_cls_delete_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_delete_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_delete_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_delete_base(vm, StackOps::FromWorkBench) }

// ── cls.count ────────────────────────────────────────────────────────────────

fn cls_count_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.count", |opts| api::add::count(opts))
}
pub fn stdlib_cls_count_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_count_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_count_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_count_base(vm, StackOps::FromWorkBench) }

// ── cls.duplicates ───────────────────────────────────────────────────────────

fn cls_duplicates_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.duplicates", |opts| api::add::duplicates(opts))
}
pub fn stdlib_cls_duplicates_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_duplicates_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_duplicates_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_duplicates_base(vm, StackOps::FromWorkBench) }

// ── cls.fingerprints.recent ──────────────────────────────────────────────────

fn cls_fingerprints_recent_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.fingerprints.recent", |dur| {
        let d = value_to_string(dur, "duration", "cls.fingerprints.recent")?;
        api::add::fingerprints_recent(&d)
    })
}
pub fn stdlib_cls_fingerprints_recent_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_fingerprints_recent_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_fingerprints_recent_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_fingerprints_recent_base(vm, StackOps::FromWorkBench) }

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    let _ = vm.vm.register_inline("cls.add".into(),                       stdlib_cls_add_stack)?;
    let _ = vm.vm.register_inline("cls.add.".into(),                      stdlib_cls_add_workbench)?;
    let _ = vm.vm.register_inline("cls.add.batch".into(),                 stdlib_cls_add_batch_stack)?;
    let _ = vm.vm.register_inline("cls.add.batch.".into(),                stdlib_cls_add_batch_workbench)?;
    let _ = vm.vm.register_inline("cls.update".into(),                    stdlib_cls_update_stack)?;
    let _ = vm.vm.register_inline("cls.update.".into(),                   stdlib_cls_update_workbench)?;
    let _ = vm.vm.register_inline("cls.delete".into(),                    stdlib_cls_delete_stack)?;
    let _ = vm.vm.register_inline("cls.delete.".into(),                   stdlib_cls_delete_workbench)?;
    let _ = vm.vm.register_inline("cls.count".into(),                     stdlib_cls_count_stack)?;
    let _ = vm.vm.register_inline("cls.count.".into(),                    stdlib_cls_count_workbench)?;
    let _ = vm.vm.register_inline("cls.duplicates".into(),                stdlib_cls_duplicates_stack)?;
    let _ = vm.vm.register_inline("cls.duplicates.".into(),               stdlib_cls_duplicates_workbench)?;
    let _ = vm.vm.register_inline("cls.fingerprints.recent".into(),       stdlib_cls_fingerprints_recent_stack)?;
    let _ = vm.vm.register_inline("cls.fingerprints.recent.".into(),      stdlib_cls_fingerprints_recent_workbench)?;
    Ok(())
}
