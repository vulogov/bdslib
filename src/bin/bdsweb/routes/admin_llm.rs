//! Administration → LLM page.
//!
//! Read-only dashboard over the v4/llm.* surface plus a single
//! mutation (cache purge).  Surfaces:
//!
//! - registered providers + their default models + capabilities
//!   (from `v4/llm.providers.list`)
//! - inference-cache stats (rows / total hits / bytes / ttl)
//!   (from `v4/llm.cache.stats`)
//! - most-recent async jobs (from `v4/llm.jobs.list`)
//!
//! All v4/* calls go through `admin::signed_rpc` so the page acts
//! under the same trust boundary as `/admin/users`.  Failures degrade
//! gracefully — the section that failed renders a small error banner
//! instead of breaking the whole page.

use askama::Template;
use axum::{
    extract::State,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::{admin::signed_rpc, error::AppError, state::AppState};

#[derive(Template)]
#[template(path = "admin_llm.html")]
struct AdminLlmPage {
    providers:    Vec<ProviderRow>,
    default_provider: String,
    cache:        CacheView,
    jobs:         Vec<JobRow>,
    error_msg:    String,
    has_error:    bool,
    notice:       String,
    has_notice:   bool,
    needs_secret: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub id:            String,
    pub default_model: String,
    pub chat:          bool,
    pub embed:         bool,
}

#[derive(Debug, Clone, Default)]
pub struct CacheView {
    pub enabled:     bool,
    pub ttl_secs:    u64,
    pub rows:        u64,
    pub total_hits:  u64,
    pub bytes_rough: u64,
    pub error:       String,
}

impl CacheView {
    pub fn has_error(&self)        -> bool { !self.error.is_empty() }
    pub fn bytes_rough_human(&self) -> String { fmt_bytes(self.bytes_rough) }
}

fn fmt_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB { format!("{:.2} GiB", n as f64 / GIB as f64) }
    else if n >= MIB { format!("{:.2} MiB", n as f64 / MIB as f64) }
    else if n >= KIB { format!("{:.1} KiB", n as f64 / KIB as f64) }
    else { format!("{n} B") }
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub job_id:       String,
    pub kind:         String,
    pub state:        String,
    pub state_class:  String,
    pub submitted_at: String,
    pub finished_at:  String,
    pub error:        String,
}

/// Bare entry point for callers that don't need banner-state — kept
/// public so the page can be reached programmatically (e.g. by a
/// future redirect-then-render flow).  The router uses
/// `page_with_banners` so query-param notices show up.
#[allow(dead_code)]
pub async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    render_page(&state, "", "").await
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    pub notice: String,
    #[serde(default)]
    pub error:  String,
}

pub async fn page_with_banners(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PageQuery>,
) -> Result<Html<String>, AppError> {
    let notice = match q.notice.as_str() {
        "purged" => "Cache purged.",
        _        => "",
    };
    render_page(&state, &q.error, notice).await
}

async fn render_page(
    state:     &AppState,
    error_msg: &str,
    notice:    &str,
) -> Result<Html<String>, AppError> {
    // Three independent reads.  Each failure is captured locally so
    // the section can render an inline error while the others still
    // surface data.
    let (providers, default_provider, providers_err) = load_providers(state).await;
    let cache = load_cache(state).await;
    let (jobs, jobs_err) = load_jobs(state).await;

    let mut combined_err = error_msg.to_owned();
    for e in [&providers_err, &jobs_err] {
        if !e.is_empty() {
            if !combined_err.is_empty() { combined_err.push_str("; "); }
            combined_err.push_str(e);
        }
    }

    Ok(Html(AdminLlmPage {
        providers,
        default_provider,
        cache,
        jobs,
        has_error: !combined_err.is_empty(),
        error_msg: combined_err,
        has_notice: !notice.is_empty(),
        notice:     notice.to_owned(),
        needs_secret: state.shared_secret.is_empty(),
    }.render()?))
}

async fn load_providers(state: &AppState) -> (Vec<ProviderRow>, String, String) {
    match signed_rpc(state, "v4/llm.providers.list", json!({})).await {
        Ok(v) => {
            let default = v.get("default").and_then(|x| x.as_str())
                .unwrap_or("").to_owned();
            let arr = v.get("providers").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            let rows: Vec<ProviderRow> = arr.into_iter().filter_map(parse_provider).collect();
            (rows, default, String::new())
        }
        Err(e) => (Vec::new(), String::new(), format!("providers: {e}")),
    }
}

fn parse_provider(v: JsonValue) -> Option<ProviderRow> {
    let id = v.get("id")?.as_str()?.to_owned();
    let default_model = v.get("default_model").and_then(|x| x.as_str())
        .unwrap_or("").to_owned();
    let caps = v.get("capabilities");
    let chat  = caps.and_then(|c| c.get("chat")) .and_then(|x| x.as_bool()).unwrap_or(false);
    let embed = caps.and_then(|c| c.get("embed")).and_then(|x| x.as_bool()).unwrap_or(false);
    Some(ProviderRow { id, default_model, chat, embed })
}

async fn load_cache(state: &AppState) -> CacheView {
    match signed_rpc(state, "v4/llm.cache.stats", json!({})).await {
        Ok(v) => CacheView {
            enabled:     v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
            ttl_secs:    v.get("ttl_secs").and_then(|x| x.as_u64()).unwrap_or(0),
            rows:        v.get("rows").and_then(|x| x.as_u64()).unwrap_or(0),
            total_hits:  v.get("total_hits").and_then(|x| x.as_u64()).unwrap_or(0),
            bytes_rough: v.get("bytes_rough").and_then(|x| x.as_u64()).unwrap_or(0),
            error:       String::new(),
        },
        Err(e) => CacheView { error: format!("cache: {e}"), ..Default::default() },
    }
}

async fn load_jobs(state: &AppState) -> (Vec<JobRow>, String) {
    match signed_rpc(state, "v4/llm.jobs.list", json!({"limit": 20})).await {
        Ok(v) => {
            let arr = v.get("jobs").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            let rows = arr.into_iter().filter_map(parse_job).collect();
            (rows, String::new())
        }
        Err(e) => (Vec::new(), format!("jobs: {e}")),
    }
}

fn parse_job(v: JsonValue) -> Option<JobRow> {
    let job_id = v.get("job_id")?.as_str()?.to_owned();
    let kind   = v.get("kind").and_then(|x| x.as_str()).unwrap_or("?").to_owned();
    let state  = v.get("state").and_then(|x| x.as_str()).unwrap_or("?").to_owned();
    let state_class = match state.as_str() {
        "done"      => "text-emerald-300",
        "failed"    => "text-red-300",
        "cancelled" => "text-amber-300",
        "running"   => "text-sky-300",
        "pending"   => "text-slate-300",
        _           => "text-slate-300",
    }.to_owned();
    let submitted_at = v.get("submitted_at").and_then(|x| x.as_u64())
        .map(crate::client::fmt_ts).unwrap_or_default();
    let finished_at  = v.get("finished_at").and_then(|x| x.as_u64())
        .map(crate::client::fmt_ts).unwrap_or_default();
    let error = v.get("error").and_then(|x| x.as_str()).unwrap_or("").to_owned();
    Some(JobRow { job_id, kind, state, state_class, submitted_at, finished_at, error })
}

// ── Cache purge form ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct PurgeForm {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub kind:     String,
    /// Operator-supplied "older than" in seconds.  0 / empty → no
    /// age filter (with no other filters, purges the whole cache).
    #[serde(default)]
    pub older_than_secs: u64,
}

pub async fn purge(
    State(state): State<AppState>,
    Form(form): Form<PurgeForm>,
) -> Response {
    let mut params = serde_json::Map::new();
    if !form.provider.is_empty() {
        params.insert("provider".into(), json!(form.provider));
    }
    if !form.kind.is_empty() {
        params.insert("kind".into(), json!(form.kind));
    }
    if form.older_than_secs > 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let cutoff = now.saturating_sub(form.older_than_secs);
        params.insert("older_than_created".into(), json!(cutoff));
    }
    match signed_rpc(&state, "v4/llm.cache.purge", JsonValue::Object(params)).await {
        Ok(_) => Redirect::to("/admin/llm?notice=purged").into_response(),
        Err(e) => Redirect::to(&format!(
            "/admin/llm?error={}",
            urlencoding::encode(&format!("purge failed: {e}"))
        )).into_response(),
    }
}
