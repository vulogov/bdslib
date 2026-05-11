//! `cls.doc*` family — fully-replicated document store.
//!
//! | Word                       | Stack (deepest first)            | Output                  |
//! |----------------------------|----------------------------------|-------------------------|
//! | `cls.doc.add`              | `metadata(MAP) content(BIN/STR)` | id (STRING)             |
//! | `cls.doc.add.file`         | `metadata(MAP) path(STR)`        | id (STRING)             |
//! | `cls.doc.update.metadata`  | `id(STR) metadata(MAP)`          | nodata                  |
//! | `cls.doc.update.content`   | `id(STR) content(BIN/STR)`       | nodata                  |
//! | `cls.doc.delete`           | `id(STR)`                        | nodata                  |
//! | `cls.doc.get.metadata`     | `id(STR)`                        | metadata Map (or null)  |
//! | `cls.doc.get.content`      | `id(STR)`                        | bytes (Binary)          |
//! | `cls.doc.search`           | `query(STR/MAP) limit(INT)`      | `{results: […]}`        |
//! | `cls.doc.search.strings`   | `query(STR/MAP) limit(INT)`      | `{results: [STR, …]}`   |
//! | `cls.doc.search.json`      | `query(MAP) limit(INT)`          | `{results: […]}`        |
//! | `cls.doc.search.json.strings` | `query(MAP) limit(INT)`       | `{results: [STR, …]}`   |
//! | `cls.doc.reindex`          | (no inputs)                      | n_reindexed (INT)       |
//! | `cls.doc.sync`             | (no inputs)                      | nodata                  |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{
    no_args, one_arg, two_args, value_to_bytes, value_to_string, value_to_usize,
};

fn cls_doc_add_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.doc.add", |metadata, content| {
        let bytes = value_to_bytes(content, "cls.doc.add")?;
        api::documents::doc_add(metadata, bytes)
    })
}
pub fn stdlib_cls_doc_add_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_add_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_add_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_add_base(vm, StackOps::FromWorkBench) }

fn cls_doc_add_file_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.doc.add.file", |metadata, path| {
        let p = value_to_string(path, "path", "cls.doc.add.file")?;
        api::documents::doc_add_file(metadata, &p)
    })
}
pub fn stdlib_cls_doc_add_file_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_add_file_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_add_file_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_add_file_base(vm, StackOps::FromWorkBench) }

fn cls_doc_update_metadata_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.doc.update.metadata", |id, metadata|
        api::documents::doc_update_metadata(id, metadata))
}
pub fn stdlib_cls_doc_update_metadata_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_update_metadata_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_update_metadata_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_update_metadata_base(vm, StackOps::FromWorkBench) }

fn cls_doc_update_content_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.doc.update.content", |id, content| {
        let bytes = value_to_bytes(content, "cls.doc.update.content")?;
        api::documents::doc_update_content(id, bytes)
    })
}
pub fn stdlib_cls_doc_update_content_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_update_content_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_update_content_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_update_content_base(vm, StackOps::FromWorkBench) }

fn cls_doc_delete_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.doc.delete", |id| api::documents::doc_delete(id))
}
pub fn stdlib_cls_doc_delete_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_delete_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_delete_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_delete_base(vm, StackOps::FromWorkBench) }

fn cls_doc_get_metadata_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.doc.get.metadata", |id| api::documents::doc_get_metadata(id))
}
pub fn stdlib_cls_doc_get_metadata_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_get_metadata_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_get_metadata_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_get_metadata_base(vm, StackOps::FromWorkBench) }

fn cls_doc_get_content_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.doc.get.content", |id| api::documents::doc_get_content(id))
}
pub fn stdlib_cls_doc_get_content_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_get_content_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_get_content_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_get_content_base(vm, StackOps::FromWorkBench) }

fn cls_doc_search_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.doc.search", |query, limit| {
        let lim = value_to_usize(limit, "limit", "cls.doc.search")?;
        api::documents::doc_search(query, lim)
    })
}
pub fn stdlib_cls_doc_search_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_search_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_search_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_search_base(vm, StackOps::FromWorkBench) }

fn cls_doc_search_strings_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.doc.search.strings", |query, limit| {
        let lim = value_to_usize(limit, "limit", "cls.doc.search.strings")?;
        api::documents::doc_search_strings(query, lim)
    })
}
pub fn stdlib_cls_doc_search_strings_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_search_strings_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_search_strings_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_search_strings_base(vm, StackOps::FromWorkBench) }

fn cls_doc_search_json_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.doc.search.json", |query, limit| {
        let lim = value_to_usize(limit, "limit", "cls.doc.search.json")?;
        api::documents::doc_search_json(query, lim)
    })
}
pub fn stdlib_cls_doc_search_json_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_search_json_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_search_json_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_search_json_base(vm, StackOps::FromWorkBench) }

fn cls_doc_search_json_strings_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.doc.search.json.strings", |query, limit| {
        let lim = value_to_usize(limit, "limit", "cls.doc.search.json.strings")?;
        api::documents::doc_search_json_strings(query, lim)
    })
}
pub fn stdlib_cls_doc_search_json_strings_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_search_json_strings_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_search_json_strings_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_search_json_strings_base(vm, StackOps::FromWorkBench) }

fn cls_doc_reindex_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    no_args(vm, op, || api::documents::doc_reindex())
}
pub fn stdlib_cls_doc_reindex_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_reindex_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_reindex_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_reindex_base(vm, StackOps::FromWorkBench) }

fn cls_doc_sync_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    no_args(vm, op, || api::documents::doc_sync())
}
pub fn stdlib_cls_doc_sync_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_doc_sync_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_doc_sync_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_doc_sync_base(vm, StackOps::FromWorkBench) }

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    macro_rules! reg {
        ($name:expr, $stack:ident, $wb:ident) => {{
            let _ = vm.vm.register_inline(format!("{}",  $name), $stack)?;
            let _ = vm.vm.register_inline(format!("{}.", $name), $wb)?;
        }};
    }
    reg!("cls.doc.add",                  stdlib_cls_doc_add_stack,                  stdlib_cls_doc_add_workbench);
    reg!("cls.doc.add.file",             stdlib_cls_doc_add_file_stack,             stdlib_cls_doc_add_file_workbench);
    reg!("cls.doc.update.metadata",      stdlib_cls_doc_update_metadata_stack,      stdlib_cls_doc_update_metadata_workbench);
    reg!("cls.doc.update.content",       stdlib_cls_doc_update_content_stack,       stdlib_cls_doc_update_content_workbench);
    reg!("cls.doc.delete",               stdlib_cls_doc_delete_stack,               stdlib_cls_doc_delete_workbench);
    reg!("cls.doc.get.metadata",         stdlib_cls_doc_get_metadata_stack,         stdlib_cls_doc_get_metadata_workbench);
    reg!("cls.doc.get.content",          stdlib_cls_doc_get_content_stack,          stdlib_cls_doc_get_content_workbench);
    reg!("cls.doc.search",               stdlib_cls_doc_search_stack,               stdlib_cls_doc_search_workbench);
    reg!("cls.doc.search.strings",       stdlib_cls_doc_search_strings_stack,       stdlib_cls_doc_search_strings_workbench);
    reg!("cls.doc.search.json",          stdlib_cls_doc_search_json_stack,          stdlib_cls_doc_search_json_workbench);
    reg!("cls.doc.search.json.strings",  stdlib_cls_doc_search_json_strings_stack,  stdlib_cls_doc_search_json_strings_workbench);
    reg!("cls.doc.reindex",              stdlib_cls_doc_reindex_stack,              stdlib_cls_doc_reindex_workbench);
    reg!("cls.doc.sync",                 stdlib_cls_doc_sync_stack,                 stdlib_cls_doc_sync_workbench);
    Ok(())
}
