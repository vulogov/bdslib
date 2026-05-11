//! `cls.tpl*` family — per-shard template store (drain3 + arbitrary).
//!
//! | Word                            | Stack (deepest first)                | Output                    |
//! |---------------------------------|--------------------------------------|---------------------------|
//! | `cls.tpl.add`                   | `metadata(MAP) body(BIN/STR)`        | id (STRING)               |
//! | `cls.tpl.update.metadata`       | `id(STR) metadata(MAP)`              | nodata                    |
//! | `cls.tpl.update.body`           | `id(STR) body(BIN/STR)`              | nodata                    |
//! | `cls.tpl.delete`                | `id(STR)`                            | nodata                    |
//! | `cls.tpl.reindex`               | `duration(STR)`                      | n_reindexed (INT)         |
//! | `cls.tpl.get`                   | `id(STR)`                            | `{id, metadata, body}`    |
//! | `cls.tpl.list`                  | `duration(STR)`                      | `{templates: […]}`        |
//! | `cls.tpl.search`                | `duration(STR) query(STR) limit(INT)`| `{results: […]}`          |
//! | `cls.tpl.template.by.id`        | `id(STR)`                            | `{template}`              |
//! | `cls.tpl.templates.recent`      | `duration(STR)`                      | `{templates: […]}`        |
//! | `cls.tpl.templates.by.timestamp`| `start_ts(INT) end_ts(INT)`          | `{templates: […]}`        |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{
    one_arg, three_args, two_args, value_to_bytes, value_to_string, value_to_u64, value_to_usize,
};

fn cls_tpl_add_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.tpl.add", |metadata, body| {
        let bytes = value_to_bytes(body, "cls.tpl.add")?;
        api::templates::tpl_add(metadata, bytes)
    })
}
pub fn stdlib_cls_tpl_add_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_add_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_add_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_add_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_update_metadata_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.tpl.update.metadata", |id, metadata|
        api::templates::tpl_update_metadata(id, metadata))
}
pub fn stdlib_cls_tpl_update_metadata_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_update_metadata_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_update_metadata_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_update_metadata_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_update_body_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.tpl.update.body", |id, body| {
        let bytes = value_to_bytes(body, "cls.tpl.update.body")?;
        api::templates::tpl_update_body(id, bytes)
    })
}
pub fn stdlib_cls_tpl_update_body_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_update_body_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_update_body_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_update_body_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_delete_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.tpl.delete", |id| api::templates::tpl_delete(id))
}
pub fn stdlib_cls_tpl_delete_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_delete_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_delete_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_delete_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_reindex_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.tpl.reindex", |dur| {
        let d = value_to_string(dur, "duration", "cls.tpl.reindex")?;
        api::templates::tpl_reindex(&d)
    })
}
pub fn stdlib_cls_tpl_reindex_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_reindex_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_reindex_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_reindex_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_get_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.tpl.get", |id| api::templates::tpl_get(id))
}
pub fn stdlib_cls_tpl_get_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_get_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_get_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_get_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_list_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.tpl.list", |dur| {
        let d = value_to_string(dur, "duration", "cls.tpl.list")?;
        api::templates::tpl_list(&d)
    })
}
pub fn stdlib_cls_tpl_list_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_list_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_list_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_list_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_search_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    three_args(vm, op, "cls.tpl.search", |duration, query, limit| {
        let dur = value_to_string(duration, "duration", "cls.tpl.search")?;
        let q   = value_to_string(query,    "query",    "cls.tpl.search")?;
        let lim = value_to_usize(limit,     "limit",    "cls.tpl.search")?;
        api::templates::tpl_search(&dur, &q, lim)
    })
}
pub fn stdlib_cls_tpl_search_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_search_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_search_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_search_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_template_by_id_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.tpl.template.by.id", |id| {
        let s = value_to_string(id, "id", "cls.tpl.template.by.id")?;
        api::templates::tpl_template_by_id(&s)
    })
}
pub fn stdlib_cls_tpl_template_by_id_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_template_by_id_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_template_by_id_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_template_by_id_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_templates_recent_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.tpl.templates.recent", |dur| {
        let d = value_to_string(dur, "duration", "cls.tpl.templates.recent")?;
        api::templates::tpl_templates_recent(&d)
    })
}
pub fn stdlib_cls_tpl_templates_recent_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_templates_recent_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_templates_recent_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_templates_recent_base(vm, StackOps::FromWorkBench) }

fn cls_tpl_templates_by_timestamp_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.tpl.templates.by.timestamp", |start_ts, end_ts| {
        let s = value_to_u64(start_ts, "start_ts", "cls.tpl.templates.by.timestamp")?;
        let e = value_to_u64(end_ts,   "end_ts",   "cls.tpl.templates.by.timestamp")?;
        api::templates::tpl_templates_by_timestamp(s, e)
    })
}
pub fn stdlib_cls_tpl_templates_by_timestamp_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_tpl_templates_by_timestamp_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_tpl_templates_by_timestamp_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_tpl_templates_by_timestamp_base(vm, StackOps::FromWorkBench) }

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    macro_rules! reg {
        ($name:expr, $stack:ident, $wb:ident) => {{
            let _ = vm.vm.register_inline(format!("{}",  $name), $stack)?;
            let _ = vm.vm.register_inline(format!("{}.", $name), $wb)?;
        }};
    }
    reg!("cls.tpl.add",                    stdlib_cls_tpl_add_stack,                    stdlib_cls_tpl_add_workbench);
    reg!("cls.tpl.update.metadata",        stdlib_cls_tpl_update_metadata_stack,        stdlib_cls_tpl_update_metadata_workbench);
    reg!("cls.tpl.update.body",            stdlib_cls_tpl_update_body_stack,            stdlib_cls_tpl_update_body_workbench);
    reg!("cls.tpl.delete",                 stdlib_cls_tpl_delete_stack,                 stdlib_cls_tpl_delete_workbench);
    reg!("cls.tpl.reindex",                stdlib_cls_tpl_reindex_stack,                stdlib_cls_tpl_reindex_workbench);
    reg!("cls.tpl.get",                    stdlib_cls_tpl_get_stack,                    stdlib_cls_tpl_get_workbench);
    reg!("cls.tpl.list",                   stdlib_cls_tpl_list_stack,                   stdlib_cls_tpl_list_workbench);
    reg!("cls.tpl.search",                 stdlib_cls_tpl_search_stack,                 stdlib_cls_tpl_search_workbench);
    reg!("cls.tpl.template.by.id",         stdlib_cls_tpl_template_by_id_stack,         stdlib_cls_tpl_template_by_id_workbench);
    reg!("cls.tpl.templates.recent",       stdlib_cls_tpl_templates_recent_stack,       stdlib_cls_tpl_templates_recent_workbench);
    reg!("cls.tpl.templates.by.timestamp", stdlib_cls_tpl_templates_by_timestamp_stack, stdlib_cls_tpl_templates_by_timestamp_workbench);
    Ok(())
}
