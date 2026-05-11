//! `cls.llm.*` Bund words — thin wrappers over `vm::api::llm::*`.
//!
//! | Word                 | Stack (deepest first) | Result                |
//! |----------------------|-----------------------|-----------------------|
//! | `cls.llm.complete`   | `req(MAP)`            | response Map          |
//! | `cls.llm.embed`      | `req(MAP)`            | embedding Map         |
//! | `cls.llm.providers`  | –                     | `{default, providers: [...]}` |
//! | `?llm.meta`          | –                     | per-thread LLM meta or `nodata` |
//!
//! `req` for `cls.llm.complete` may include any of:
//! `{provider?, model?, prompt?, messages?, options?}` — `prompt` is a
//! shortcut for a single user message.
//!
//! `req` for `cls.llm.embed`: `{provider?, model?, text? | texts?}`.

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_dynamic::value::Value;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::helpers::eval::json_to_dynamic;
use crate::vm::stdlib::cluster::helpers::{no_args, one_arg, push};

// ── cls.llm.complete ─────────────────────────────────────────────────

fn cls_llm_complete_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.complete", api::llm::complete)
}
pub fn stdlib_cls_llm_complete_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_complete_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_complete_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_complete_base(vm, StackOps::FromWorkBench) }

// ── cls.llm.chat ─────────────────────────────────────────────────────

fn cls_llm_chat_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.chat", api::llm::chat)
}
pub fn stdlib_cls_llm_chat_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_chat_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_chat_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_chat_base(vm, StackOps::FromWorkBench) }

// ── cls.llm.analyze ──────────────────────────────────────────────────

fn cls_llm_analyze_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.analyze", api::llm::analyze)
}
pub fn stdlib_cls_llm_analyze_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_analyze_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_analyze_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_analyze_base(vm, StackOps::FromWorkBench) }

// ── cls.llm.embed ────────────────────────────────────────────────────

fn cls_llm_embed_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.embed", api::llm::embed)
}
pub fn stdlib_cls_llm_embed_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_embed_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_embed_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_embed_base(vm, StackOps::FromWorkBench) }

// ── cls.llm.complete.async ───────────────────────────────────────────

fn cls_llm_complete_async_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.complete.async", api::llm::complete_async)
}
pub fn stdlib_cls_llm_complete_async_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_complete_async_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_complete_async_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_complete_async_base(vm, StackOps::FromWorkBench) }

// ── cls.llm.analyze.async ────────────────────────────────────────────

fn cls_llm_analyze_async_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.analyze.async", api::llm::analyze_async)
}
pub fn stdlib_cls_llm_analyze_async_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_analyze_async_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_analyze_async_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_analyze_async_base(vm, StackOps::FromWorkBench) }

// ── cls.llm.jobs.list / status / cancel ──────────────────────────────

fn cls_llm_jobs_list_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.jobs.list", api::llm::jobs_list)
}
pub fn stdlib_cls_llm_jobs_list_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_jobs_list_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_jobs_list_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_jobs_list_base(vm, StackOps::FromWorkBench) }

fn cls_llm_status_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.status", api::llm::job_status)
}
pub fn stdlib_cls_llm_status_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_status_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_status_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_status_base(vm, StackOps::FromWorkBench) }

fn cls_llm_cancel_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.llm.cancel", api::llm::job_cancel)
}
pub fn stdlib_cls_llm_cancel_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_cancel_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_cancel_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_cancel_base(vm, StackOps::FromWorkBench) }

// ── cls.llm.providers ────────────────────────────────────────────────

fn cls_llm_providers_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    no_args(vm, op, api::llm::providers_list)
}
pub fn stdlib_cls_llm_providers_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_llm_providers_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_llm_providers_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_llm_providers_base(vm, StackOps::FromWorkBench) }

// ── ?llm.meta ────────────────────────────────────────────────────────

fn llm_meta_value() -> Value {
    match api::meta::get_llm() {
        Some(m) => json_to_dynamic(m),
        None    => Value::nodata(),
    }
}

pub fn stdlib_llm_meta_stack(vm: &mut VM) -> Result<&mut VM, Error> {
    push(vm, &StackOps::FromStack, llm_meta_value());
    Ok(vm)
}

pub fn stdlib_llm_meta_workbench(vm: &mut VM) -> Result<&mut VM, Error> {
    push(vm, &StackOps::FromWorkBench, llm_meta_value());
    Ok(vm)
}

// ── registration ─────────────────────────────────────────────────────

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    let _ = vm.vm.register_inline("cls.llm.complete".into(),    stdlib_cls_llm_complete_stack)?;
    let _ = vm.vm.register_inline("cls.llm.complete.".into(),   stdlib_cls_llm_complete_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.chat".into(),        stdlib_cls_llm_chat_stack)?;
    let _ = vm.vm.register_inline("cls.llm.chat.".into(),       stdlib_cls_llm_chat_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.analyze".into(),     stdlib_cls_llm_analyze_stack)?;
    let _ = vm.vm.register_inline("cls.llm.analyze.".into(),    stdlib_cls_llm_analyze_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.embed".into(),       stdlib_cls_llm_embed_stack)?;
    let _ = vm.vm.register_inline("cls.llm.embed.".into(),      stdlib_cls_llm_embed_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.complete.async".into(),  stdlib_cls_llm_complete_async_stack)?;
    let _ = vm.vm.register_inline("cls.llm.complete.async.".into(), stdlib_cls_llm_complete_async_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.analyze.async".into(),   stdlib_cls_llm_analyze_async_stack)?;
    let _ = vm.vm.register_inline("cls.llm.analyze.async.".into(),  stdlib_cls_llm_analyze_async_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.jobs.list".into(),       stdlib_cls_llm_jobs_list_stack)?;
    let _ = vm.vm.register_inline("cls.llm.jobs.list.".into(),      stdlib_cls_llm_jobs_list_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.status".into(),          stdlib_cls_llm_status_stack)?;
    let _ = vm.vm.register_inline("cls.llm.status.".into(),         stdlib_cls_llm_status_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.cancel".into(),          stdlib_cls_llm_cancel_stack)?;
    let _ = vm.vm.register_inline("cls.llm.cancel.".into(),         stdlib_cls_llm_cancel_workbench)?;
    let _ = vm.vm.register_inline("cls.llm.providers".into(),   stdlib_cls_llm_providers_stack)?;
    let _ = vm.vm.register_inline("cls.llm.providers.".into(),  stdlib_cls_llm_providers_workbench)?;
    let _ = vm.vm.register_inline("?llm.meta".into(),           stdlib_llm_meta_stack)?;
    let _ = vm.vm.register_inline("?llm.meta.".into(),          stdlib_llm_meta_workbench)?;
    Ok(())
}
