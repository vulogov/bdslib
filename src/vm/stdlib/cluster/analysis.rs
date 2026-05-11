//! `cls.*` analysis surface — anomaly / denoise / kNN / RCA / topics /
//! trends / summaries / timeline.
//!
//! Stack layouts (deepest first):
//!
//! | Word                       | Inputs                                    | Output                       |
//! |----------------------------|-------------------------------------------|------------------------------|
//! | `cls.anomaly.recent`       | `duration(STR) opts(MAP)`                 | analysis Map                 |
//! | `cls.denoise.recent`       | `duration(STR) opts(MAP)`                 | analysis Map                 |
//! | `cls.knn`                  | `duration(STR) opts(MAP)`                 | analysis Map                 |
//! | `cls.rca`                  | `opts(MAP)`                               | RCA Map                      |
//! | `cls.rca.templates`        | `opts(MAP)`                               | RCA Map                      |
//! | `cls.topics`               | `opts(MAP)` (must contain `key`+`duration`) | LDA topic Map              |
//! | `cls.topics.all`           | `opts(MAP)` (must contain `duration`)     | `{topics: […]}`              |
//! | `cls.trends`               | `key(STR) duration(STR)`                  | trend stats Map              |
//! | `cls.textrank.templates`   | `duration(STR) opts(MAP)`                 | summary Map                  |
//! | `cls.summary.recent`       | `duration(STR) opts(MAP)`                 | summary Map                  |
//! | `cls.summary.query`        | `query(STR) opts(MAP)`                    | summary Map                  |
//! | `cls.summary.lsa.recent`   | `duration(STR) opts(MAP)`                 | summary Map                  |
//! | `cls.summary.lsa.query`    | `query(STR) opts(MAP)`                    | summary Map                  |
//! | `cls.timeline`             | (no inputs)                               | `{min_ts, max_ts}`           |

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_dynamic::value::Value;
use rust_multistackvm::multistackvm::{StackOps, VM};

use crate::vm::api;
use crate::vm::stdlib::cluster::helpers::{
    no_args, one_arg, two_args, value_to_string,
};

// ── analysis-with-fingerprints (anomaly / denoise / knn) ────────────────────

fn cls_anomaly_recent_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.anomaly.recent", |duration, opts| {
        let dur = value_to_string(duration, "duration", "cls.anomaly.recent")?;
        api::analysis::anomaly_recent(&dur, opts)
    })
}
pub fn stdlib_cls_anomaly_recent_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_anomaly_recent_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_anomaly_recent_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_anomaly_recent_base(vm, StackOps::FromWorkBench) }

fn cls_denoise_recent_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.denoise.recent", |duration, opts| {
        let dur = value_to_string(duration, "duration", "cls.denoise.recent")?;
        api::analysis::denoise_recent(&dur, opts)
    })
}
pub fn stdlib_cls_denoise_recent_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_denoise_recent_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_denoise_recent_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_denoise_recent_base(vm, StackOps::FromWorkBench) }

fn cls_knn_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.knn", |duration, opts| {
        let dur = value_to_string(duration, "duration", "cls.knn")?;
        api::analysis::knn(&dur, opts)
    })
}
pub fn stdlib_cls_knn_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_knn_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_knn_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_knn_base(vm, StackOps::FromWorkBench) }

// ── RCA ──────────────────────────────────────────────────────────────────────

fn cls_rca_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.rca", |opts| api::analysis::rca(opts))
}
pub fn stdlib_cls_rca_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_rca_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_rca_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_rca_base(vm, StackOps::FromWorkBench) }

fn cls_rca_templates_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.rca.templates", |opts| api::analysis::rca_templates(opts))
}
pub fn stdlib_cls_rca_templates_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_rca_templates_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_rca_templates_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_rca_templates_base(vm, StackOps::FromWorkBench) }

// ── Topics ──────────────────────────────────────────────────────────────────

fn cls_topics_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.topics", |opts| api::analysis::topics(opts))
}
pub fn stdlib_cls_topics_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_topics_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_topics_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_topics_base(vm, StackOps::FromWorkBench) }

fn cls_topics_all_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    one_arg(vm, op, "cls.topics.all", |opts| api::analysis::topics_all(opts))
}
pub fn stdlib_cls_topics_all_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_topics_all_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_topics_all_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_topics_all_base(vm, StackOps::FromWorkBench) }

// ── Trends ──────────────────────────────────────────────────────────────────

fn cls_trends_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.trends", |key, duration| {
        let k   = value_to_string(key,      "key",      "cls.trends")?;
        let dur = value_to_string(duration, "duration", "cls.trends")?;
        api::analysis::trends(&k, &dur)
    })
}
pub fn stdlib_cls_trends_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_trends_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_trends_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_trends_base(vm, StackOps::FromWorkBench) }

// ── Summaries ───────────────────────────────────────────────────────────────

fn cls_textrank_templates_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.textrank.templates", |duration, opts| {
        let dur = value_to_string(duration, "duration", "cls.textrank.templates")?;
        api::analysis::textrank_templates(&dur, opts)
    })
}
pub fn stdlib_cls_textrank_templates_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_textrank_templates_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_textrank_templates_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_textrank_templates_base(vm, StackOps::FromWorkBench) }

fn cls_summary_recent_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.summary.recent", |duration, opts| {
        let dur = value_to_string(duration, "duration", "cls.summary.recent")?;
        api::analysis::summary_for_recent(&dur, opts)
    })
}
pub fn stdlib_cls_summary_recent_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_summary_recent_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_summary_recent_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_summary_recent_base(vm, StackOps::FromWorkBench) }

fn cls_summary_query_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.summary.query", |query, opts| {
        let q = value_to_string(query, "query", "cls.summary.query")?;
        api::analysis::summary_for_query(&q, opts)
    })
}
pub fn stdlib_cls_summary_query_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_summary_query_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_summary_query_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_summary_query_base(vm, StackOps::FromWorkBench) }

fn cls_summary_lsa_recent_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.summary.lsa.recent", |duration, opts| {
        let dur = value_to_string(duration, "duration", "cls.summary.lsa.recent")?;
        api::analysis::summary_lsa_for_recent(&dur, opts)
    })
}
pub fn stdlib_cls_summary_lsa_recent_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_summary_lsa_recent_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_summary_lsa_recent_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_summary_lsa_recent_base(vm, StackOps::FromWorkBench) }

fn cls_summary_lsa_query_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    two_args(vm, op, "cls.summary.lsa.query", |query, opts| {
        let q = value_to_string(query, "query", "cls.summary.lsa.query")?;
        api::analysis::summary_lsa_for_query(&q, opts)
    })
}
pub fn stdlib_cls_summary_lsa_query_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_summary_lsa_query_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_summary_lsa_query_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_summary_lsa_query_base(vm, StackOps::FromWorkBench) }

// ── Timeline (no args) ──────────────────────────────────────────────────────

fn cls_timeline_base<'a>(vm: &'a mut VM, op: StackOps) -> Result<&'a mut VM, Error> {
    no_args(vm, op, || api::analysis::timeline())
}
pub fn stdlib_cls_timeline_stack(vm: &mut VM)     -> Result<&mut VM, Error> { cls_timeline_base(vm, StackOps::FromStack) }
pub fn stdlib_cls_timeline_workbench(vm: &mut VM) -> Result<&mut VM, Error> { cls_timeline_base(vm, StackOps::FromWorkBench) }

// silence unused warning when only one variant is referenced from tests
#[allow(dead_code)] fn _unused(_: Value) {}

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    macro_rules! reg {
        ($name:expr, $stack:ident, $wb:ident) => {{
            let _ = vm.vm.register_inline(format!("{}",  $name), $stack)?;
            let _ = vm.vm.register_inline(format!("{}.", $name), $wb)?;
        }};
    }
    reg!("cls.anomaly.recent",      stdlib_cls_anomaly_recent_stack,      stdlib_cls_anomaly_recent_workbench);
    reg!("cls.denoise.recent",      stdlib_cls_denoise_recent_stack,      stdlib_cls_denoise_recent_workbench);
    reg!("cls.knn",                 stdlib_cls_knn_stack,                 stdlib_cls_knn_workbench);
    reg!("cls.rca",                 stdlib_cls_rca_stack,                 stdlib_cls_rca_workbench);
    reg!("cls.rca.templates",       stdlib_cls_rca_templates_stack,       stdlib_cls_rca_templates_workbench);
    reg!("cls.topics",              stdlib_cls_topics_stack,              stdlib_cls_topics_workbench);
    reg!("cls.topics.all",          stdlib_cls_topics_all_stack,          stdlib_cls_topics_all_workbench);
    reg!("cls.trends",              stdlib_cls_trends_stack,              stdlib_cls_trends_workbench);
    reg!("cls.textrank.templates",  stdlib_cls_textrank_templates_stack,  stdlib_cls_textrank_templates_workbench);
    reg!("cls.summary.recent",      stdlib_cls_summary_recent_stack,      stdlib_cls_summary_recent_workbench);
    reg!("cls.summary.query",       stdlib_cls_summary_query_stack,       stdlib_cls_summary_query_workbench);
    reg!("cls.summary.lsa.recent",  stdlib_cls_summary_lsa_recent_stack,  stdlib_cls_summary_lsa_recent_workbench);
    reg!("cls.summary.lsa.query",   stdlib_cls_summary_lsa_query_stack,   stdlib_cls_summary_lsa_query_workbench);
    reg!("cls.timeline",            stdlib_cls_timeline_stack,            stdlib_cls_timeline_workbench);
    Ok(())
}
