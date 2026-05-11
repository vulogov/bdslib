//! `cls.search` family — vector / FTS / aggregation / fulltext.
//!
//! | Word                   | Stack (deepest first)                       | Result                       |
//! |------------------------|----------------------------------------------|------------------------------|
//! | `cls.search`           | `query(*) duration(STR)`                     | `{results: [doc, …]}`        |
//! | `cls.search.get`       | `query(*) duration(STR) limit(INT)`          | `{results: [doc, …]}`        |
//! | `cls.search.fts`       | `query(STR) duration(STR)`                   | `{results: [doc, …]}`        |
//! | `cls.aggregation`      | `query(STR) duration(STR)`                   | `{observability, documents}` |
//! | `cls.fulltext`         | `query(STR) duration(STR) limit(INT)`        | `{results: [{id, score}, …]}`|
//! | `cls.fulltext.recent`  | `query(STR) duration(STR) limit(INT)`        | `{results: [{id, ts, score}]}`|
//! | `cls.fulltext.get`     | `query(STR) duration(STR) limit(INT)`        | `{results: [doc, …]}`        |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{
    three_args, two_args, value_to_string, value_to_usize,
};

// ── cls.search (vector) ──────────────────────────────────────────────────────

fn cls_search_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.search", |query, duration| {
        let dur = value_to_string(duration, "duration", "cls.search")?;
        api::search::search_vector(&dur, query)
    })
}
pub fn stdlib_cls_search_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_search_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_search_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_search_base(vm, StackOps::FromWorkBench) }

// ── cls.search.get ───────────────────────────────────────────────────────────

fn cls_search_get_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    three_args(vm, op, "cls.search.get", |query, duration, limit| {
        let dur = value_to_string(duration, "duration", "cls.search.get")?;
        let lim = value_to_usize(limit, "limit", "cls.search.get")?;
        api::search::search_get(&dur, query, lim)
    })
}
pub fn stdlib_cls_search_get_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_search_get_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_search_get_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_search_get_base(vm, StackOps::FromWorkBench) }

// ── cls.search.fts ───────────────────────────────────────────────────────────

fn cls_search_fts_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.search.fts", |query, duration| {
        let q   = value_to_string(query,    "query",    "cls.search.fts")?;
        let dur = value_to_string(duration, "duration", "cls.search.fts")?;
        api::search::search_fts(&dur, &q)
    })
}
pub fn stdlib_cls_search_fts_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_search_fts_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_search_fts_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_search_fts_base(vm, StackOps::FromWorkBench) }

// ── cls.aggregation ──────────────────────────────────────────────────────────

fn cls_aggregation_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.aggregation", |query, duration| {
        let q   = value_to_string(query,    "query",    "cls.aggregation")?;
        let dur = value_to_string(duration, "duration", "cls.aggregation")?;
        api::search::aggregation_search(&dur, &q)
    })
}
pub fn stdlib_cls_aggregation_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_aggregation_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_aggregation_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_aggregation_base(vm, StackOps::FromWorkBench) }

// ── cls.fulltext ─────────────────────────────────────────────────────────────

fn cls_fulltext_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    three_args(vm, op, "cls.fulltext", |query, duration, limit| {
        let q   = value_to_string(query,    "query",    "cls.fulltext")?;
        let dur = value_to_string(duration, "duration", "cls.fulltext")?;
        let lim = value_to_usize(limit,     "limit",    "cls.fulltext")?;
        api::search::fulltext(&dur, &q, lim)
    })
}
pub fn stdlib_cls_fulltext_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_fulltext_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_fulltext_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_fulltext_base(vm, StackOps::FromWorkBench) }

// ── cls.fulltext.recent ──────────────────────────────────────────────────────

fn cls_fulltext_recent_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    three_args(vm, op, "cls.fulltext.recent", |query, duration, limit| {
        let q   = value_to_string(query,    "query",    "cls.fulltext.recent")?;
        let dur = value_to_string(duration, "duration", "cls.fulltext.recent")?;
        let lim = value_to_usize(limit,     "limit",    "cls.fulltext.recent")?;
        api::search::fulltext_recent(&dur, &q, lim)
    })
}
pub fn stdlib_cls_fulltext_recent_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_fulltext_recent_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_fulltext_recent_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_fulltext_recent_base(vm, StackOps::FromWorkBench) }

// ── cls.fulltext.get ─────────────────────────────────────────────────────────

fn cls_fulltext_get_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    three_args(vm, op, "cls.fulltext.get", |query, duration, limit| {
        let q   = value_to_string(query,    "query",    "cls.fulltext.get")?;
        let dur = value_to_string(duration, "duration", "cls.fulltext.get")?;
        let lim = value_to_usize(limit,     "limit",    "cls.fulltext.get")?;
        api::search::fulltext_get(&dur, &q, lim)
    })
}
pub fn stdlib_cls_fulltext_get_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_fulltext_get_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_fulltext_get_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_fulltext_get_base(vm, StackOps::FromWorkBench) }

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    let _ = vm.vm.register_inline("cls.search".into(),               stdlib_cls_search_stack)?;
    let _ = vm.vm.register_inline("cls.search.".into(),              stdlib_cls_search_workbench)?;
    let _ = vm.vm.register_inline("cls.search.get".into(),           stdlib_cls_search_get_stack)?;
    let _ = vm.vm.register_inline("cls.search.get.".into(),          stdlib_cls_search_get_workbench)?;
    let _ = vm.vm.register_inline("cls.search.fts".into(),           stdlib_cls_search_fts_stack)?;
    let _ = vm.vm.register_inline("cls.search.fts.".into(),          stdlib_cls_search_fts_workbench)?;
    let _ = vm.vm.register_inline("cls.aggregation".into(),          stdlib_cls_aggregation_stack)?;
    let _ = vm.vm.register_inline("cls.aggregation.".into(),         stdlib_cls_aggregation_workbench)?;
    let _ = vm.vm.register_inline("cls.fulltext".into(),             stdlib_cls_fulltext_stack)?;
    let _ = vm.vm.register_inline("cls.fulltext.".into(),            stdlib_cls_fulltext_workbench)?;
    let _ = vm.vm.register_inline("cls.fulltext.recent".into(),      stdlib_cls_fulltext_recent_stack)?;
    let _ = vm.vm.register_inline("cls.fulltext.recent.".into(),     stdlib_cls_fulltext_recent_workbench)?;
    let _ = vm.vm.register_inline("cls.fulltext.get".into(),         stdlib_cls_fulltext_get_stack)?;
    let _ = vm.vm.register_inline("cls.fulltext.get.".into(),        stdlib_cls_fulltext_get_workbench)?;
    Ok(())
}
