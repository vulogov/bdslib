//! Administration → Configuration page.
//!
//! Read-only view over `v2/configuration` — the bdsnode hjson file
//! parsed to JSON plus a library-defaults block.  Unauthenticated
//! v2/* call for the page itself; the "Analyze this!" button rides on
//! HMAC-signed `v4/llm.analyze` (same trust boundary as the LLM admin
//! page / chat).
//!
//! Renders the two trees as pretty-printed JSON inside `<pre>` blocks.
//! "Analyze this!" pipes the entire `v2/configuration` payload as a
//! single supplied row into `v4/llm.analyze` with the default LLM
//! provider — operator's bds.hjson gets cross-referenced against
//! bdslib's documented operating envelope (BDSCONFIG.md +
//! BDSNODE_TUNING_GUIDE.md + RETENTION.md + …).  Mirrors the
//! `/perf/analyze` pattern.

use askama::Template;
use axum::{extract::State, response::Html};
use serde_json::{json, Value};

use crate::{
    admin::signed_rpc_with_timeout,
    client::{analyze_provider, rpc},
    error::AppError,
    state::AppState,
};

#[derive(Template)]
#[template(path = "admin_configuration.html")]
struct AdminConfigurationPage {
    config_path:    String,
    exists:         bool,
    loaded_at:      String,
    config_json:    String,
    defaults_json:  String,
    error_msg:      String,
    has_error:      bool,
    /// Default LLM provider id (rendered into the "Analyze this!" pane
    /// header so the operator sees which model is about to be asked).
    analyze_provider: String,
    /// Default model name for that provider — same purpose.
    analyze_model:    String,
}

pub async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    // Run the v2/configuration RPC + analyze-provider lookup concurrently.
    let (cfg_resp, (analyze_provider, analyze_model)) = tokio::join!(
        rpc(&state, "v2/configuration", Value::Null),
        analyze_provider(&state),
    );

    let (config_path, exists, loaded_at, config_json, defaults_json, error_msg) =
        match cfg_resp {
            Ok(resp) => {
                let config_path = resp.get("config_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(not set)")
                    .to_owned();
                let exists = resp.get("exists")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let loaded_at_unix = resp.get("loaded_at_unix")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let loaded_at = crate::client::fmt_ts(loaded_at_unix);
                let config_json = pretty(resp.get("config").unwrap_or(&Value::Null));
                let defaults_json = pretty(resp.get("defaults").unwrap_or(&Value::Null));
                (config_path, exists, loaded_at, config_json, defaults_json, String::new())
            }
            Err(e) => (
                String::new(),
                false,
                "—".to_owned(),
                "{}".to_owned(),
                "{}".to_owned(),
                format!("v2/configuration failed: {e}"),
            ),
        };

    let has_error = !error_msg.is_empty();
    Ok(Html(AdminConfigurationPage {
        config_path,
        exists,
        loaded_at,
        config_json,
        defaults_json,
        error_msg,
        has_error,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX: "Analyze this!" — one-shot LLM analysis of the live config ─────────

#[derive(Template)]
#[template(path = "partials/configuration_analysis.html")]
struct ConfigurationAnalysis {
    response:        String,
    response_html:   String,
    provider:        String,
    model:           String,
    ms:              u64,
    /// One of: `"miss"`, `"hit"`, `"disabled"`.
    cache:           String,
    error:           String,
}

pub async fn analyze(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let cfg = state.configuration_analyze.clone();

    // Re-fetch v2/configuration so the LLM sees the same payload the
    // page just displayed.  v2/configuration re-reads the hjson file
    // on every call, so a config edit done while bdsweb was up is
    // reflected immediately.
    let cfg_resp = match rpc(&state, "v2/configuration", Value::Null).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(ConfigurationAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms: 0,
            cache: String::new(),
            error: format!("Could not fetch configuration for analysis: {e}"),
        }.render()?)),
    };

    // The whole v2/configuration response is the analysis payload.
    // Pass it as a single supplied row — same pattern as
    // `/perf/analyze` (one snapshot blob, not a row stream).
    // `query` is set to a stable label so the cache key is useful:
    // re-analyzing the same config returns a cache hit.
    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            [cfg_resp],
            "query":           "bdsnode configuration review",
            "prompt_template": cfg.prompt_template,
        }),
        Some(std::time::Duration::from_secs(cfg.timeout_secs)),
    ).await;

    match analyze_resp {
        Ok(v) => {
            let response = v.get("response").and_then(|x| x.as_str()).unwrap_or("").to_owned();
            let provider = v.get("provider").and_then(|x| x.as_str()).unwrap_or("?").to_owned();
            let model    = v.get("model").and_then(|x| x.as_str()).unwrap_or("?").to_owned();
            let ms       = v.get("ms").and_then(|x| x.as_u64()).unwrap_or(0);
            let cache    = v.get("cache").and_then(|x| x.as_str()).unwrap_or("").to_owned();
            Ok(Html(ConfigurationAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                cache,
                error: String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(ConfigurationAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms: 0,
            cache: String::new(),
            error: format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_owned())
}
