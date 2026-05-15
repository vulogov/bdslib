//! Provider registry + config loader + process-wide singleton.
//!
//! The manager reads the `llm` block from `bds.hjson`, builds one
//! `Arc<dyn Provider>` per configured upstream, picks a default, and
//! exposes them by name.  Calls into the manager are infallible once
//! the provider is registered; lookups for an unknown name return a
//! plain `Err` so the caller can surface it.
//!
//! Providers requiring an API key read the key from the env var named
//! by `api_key_env` (default: `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`).
//! If the env var is unset at startup the provider is logged + skipped
//! rather than failing the whole process — nodes without that
//! provider's credentials still come up clean with the others.

use crate::common::error::{err_msg, Result};
use crate::llm::providers::{
    AnthropicProvider, DeepSeekProvider, GeminiProvider, OllamaProvider, OpenAIProvider, Provider,
};
use serde_hjson::Value as HjsonValue;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

const DEFAULT_OLLAMA_URL:           &str = "http://localhost:11434";
const DEFAULT_OLLAMA_MODEL:         &str = "llama3.2";
const DEFAULT_ANTHROPIC_URL:        &str = "https://api.anthropic.com";
const DEFAULT_ANTHROPIC_MODEL:      &str = "claude-sonnet-4-5";
const DEFAULT_ANTHROPIC_KEY_ENV:    &str = "ANTHROPIC_API_KEY";
const DEFAULT_OPENAI_URL:           &str = "https://api.openai.com";
const DEFAULT_OPENAI_MODEL:         &str = "gpt-4o-mini";
const DEFAULT_OPENAI_KEY_ENV:       &str = "OPENAI_API_KEY";
const DEFAULT_DEEPSEEK_URL:         &str = "https://api.deepseek.com";
const DEFAULT_DEEPSEEK_MODEL:       &str = "deepseek-chat";
const DEFAULT_DEEPSEEK_KEY_ENV:     &str = "DEEPSEEK_API_KEY";
const DEFAULT_GEMINI_URL:           &str = "https://generativelanguage.googleapis.com";
const DEFAULT_GEMINI_MODEL:         &str = "gemini-2.5-flash";
const DEFAULT_GEMINI_KEY_ENV:       &str = "GEMINI_API_KEY";

#[derive(Debug, Clone)]
pub struct OllamaConfig    { pub url: String,      pub default_model: String }

#[derive(Debug, Clone)]
pub struct AnthropicConfig { pub base_url: String, pub api_key_env: String, pub default_model: String }

#[derive(Debug, Clone)]
pub struct OpenAIConfig    { pub base_url: String, pub api_key_env: String, pub default_model: String }

/// DeepSeek (`llm.providers.deepseek.*`).  Unlike [`AnthropicConfig`]
/// and [`OpenAIConfig`], DeepSeek allows the key to be supplied either
/// via the env var named by `api_key_env` (preferred) **or** via the
/// `api_key` field directly in hjson.  The env var takes precedence
/// when both are present.  `api_key` is `String::new()` when absent.
#[derive(Debug, Clone)]
pub struct DeepSeekConfig {
    pub base_url:      String,
    pub api_key_env:   String,
    pub api_key:       String,
    pub default_model: String,
}

/// Google Gemini (`llm.providers.gemini.*`).  Same env-or-hjson key
/// resolution as [`DeepSeekConfig`] — env var (preferred) wins over
/// the plaintext `api_key` field when both are present.  The provider
/// sends the key via the `x-goog-api-key` header so it never lands
/// in HTTP access logs.
#[derive(Debug, Clone)]
pub struct GeminiConfig {
    pub base_url:      String,
    pub api_key_env:   String,
    pub api_key:       String,
    pub default_model: String,
}

/// Cache sub-block (`llm.cache.*` in bds.hjson).
///
/// `enabled` defaults to `true`; `ttl_secs` defaults to 24 hours.  Both
/// can be overridden per call: setting `cache: false` in the request
/// bypasses the cache regardless of the global default (used for
/// non-deterministic `temperature > 0` workflows).
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub enabled:  bool,
    pub ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { enabled: true, ttl_secs: 86_400 }
    }
}

/// Dedup sub-block (`llm.dedup.*` in bds.hjson).  Controls the
/// cluster-wide single-execution behaviour driven by `InferenceLog`.
///
/// `window_secs` — how long a recent `done` / `failed` row keeps
/// short-circuiting fresh requests; matches Risk #2-style semantics
/// (within this window, the inference cache should already have the
/// answer, so a repeat request grabs that instead of re-running).
///
/// `wait_max_secs` — how long a sync caller polls the inference cache
/// for a peer's in-progress result before falling through and running
/// the inference itself.  Set to 0 to "fail fast" (return immediately
/// instead of waiting).
#[derive(Debug, Clone)]
pub struct DedupConfig {
    pub enabled:       bool,
    pub window_secs:   u64,
    pub wait_max_secs: u64,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self { enabled: true, window_secs: 300, wait_max_secs: 30 }
    }
}

/// `llm.chat.bund.*` — controls Bund-snippet evaluation embedded
/// inside chat messages.  See [`crate::llm::snippet`] for the detector
/// + [`Documentation/LLM.md`] for the feature spec.
///
/// `enabled` defaults to **false** — this is a privilege grant
/// (anyone with chat access can run arbitrary Bund), operators must
/// opt in explicitly.
///
/// `timeout_secs` floor 1, ceiling 60.  `max_result_chars` is the
/// JSON-encoded size cap before the oversize strategy fires.
#[derive(Debug, Clone)]
pub struct ChatBundConfig {
    pub enabled:           bool,
    pub timeout_secs:      u64,
    pub max_result_chars:  usize,
    /// One of `"fingerprint"` / `"truncate"` / `"drop"`.  See LLM.md.
    pub oversize_strategy: String,
    /// `"strict"` (default) or `"permissive"` — see `snippet::SlashStrictness`.
    pub slash_strictness:  String,
    pub fenced_only:       bool,
}

impl Default for ChatBundConfig {
    fn default() -> Self {
        Self {
            enabled:           false,
            timeout_secs:      10,
            max_result_chars:  16_384,
            oversize_strategy: "fingerprint".to_owned(),
            slash_strictness:  "strict".to_owned(),
            fenced_only:       false,
        }
    }
}

/// `llm.chat.*` parent block.  Currently only holds `bund` but kept
/// as its own struct so future chat-specific knobs nest cleanly.
#[derive(Debug, Clone, Default)]
pub struct ChatConfig {
    pub bund: ChatBundConfig,
}

/// `llm.to_bund.*` — controls the v2/to.bund English → Bund
/// translator.  See [`crate::llm::to_bund`].
///
/// `enabled` defaults to `true` (the feature ships on); operators
/// disable it when they don't want users to spend tokens on
/// translation.  `timeout_secs` is the per-request reqwest timeout
/// passed through to the provider — bigger than `llm.chat.bund`
/// because the system prompt alone is ~15k chars.  `max_retries`
/// is the number of additional parse-fix turns after the first
/// attempt fails to parse; ceiling 5.
#[derive(Debug, Clone)]
pub struct ToBundConfig {
    pub enabled:             bool,
    pub timeout_secs:        u64,
    pub max_retries:         usize,
    pub provider:            String,
    pub model:               String,
    pub extra_system_prompt: String,
}

impl Default for ToBundConfig {
    fn default() -> Self {
        Self {
            enabled:             true,
            timeout_secs:        120,
            max_retries:         2,
            provider:            String::new(),
            model:               String::new(),
            extra_system_prompt: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LlmConfig {
    pub default:   Option<String>,
    pub ollama:    Option<OllamaConfig>,
    pub anthropic: Option<AnthropicConfig>,
    pub openai:    Option<OpenAIConfig>,
    pub deepseek:  Option<DeepSeekConfig>,
    pub gemini:    Option<GeminiConfig>,
    pub cache:     CacheConfig,
    pub dedup:     DedupConfig,
    pub chat:      ChatConfig,
    pub to_bund:   ToBundConfig,
}

impl LlmConfig {
    /// Parse the `llm` block of `bds.hjson`.  Returns an empty config
    /// (no providers) when the file is missing, unparseable, or has no
    /// `llm` key — callers decide whether that's fatal.
    pub fn load_from_hjson(path: &str) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(r)  => r,
            Err(_) => return Self::default(),
        };
        let val: HjsonValue = match serde_hjson::from_str(&raw) {
            Ok(v)  => v,
            Err(_) => return Self::default(),
        };
        let obj = match val.as_object() {
            Some(o) => o,
            None    => return Self::default(),
        };
        let llm = match obj.get("llm").and_then(|v| v.as_object()) {
            Some(o) => o,
            None    => return Self::default(),
        };

        let default = llm.get("default")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let providers = llm.get("providers").and_then(|v| v.as_object());

        let ollama = providers
            .and_then(|p| p.get("ollama").and_then(|v| v.as_object()))
            .map(|o| OllamaConfig {
                url:           o.get("url").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_OLLAMA_URL).to_owned(),
                default_model: o.get("default_model").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_OLLAMA_MODEL).to_owned(),
            });

        let anthropic = providers
            .and_then(|p| p.get("anthropic").and_then(|v| v.as_object()))
            .map(|o| AnthropicConfig {
                base_url:      o.get("base_url").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_ANTHROPIC_URL).to_owned(),
                api_key_env:   o.get("api_key_env").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_ANTHROPIC_KEY_ENV).to_owned(),
                default_model: o.get("default_model").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_ANTHROPIC_MODEL).to_owned(),
            });

        let openai = providers
            .and_then(|p| p.get("openai").and_then(|v| v.as_object()))
            .map(|o| OpenAIConfig {
                base_url:      o.get("base_url").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_OPENAI_URL).to_owned(),
                api_key_env:   o.get("api_key_env").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_OPENAI_KEY_ENV).to_owned(),
                default_model: o.get("default_model").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_OPENAI_MODEL).to_owned(),
            });

        let deepseek = providers
            .and_then(|p| p.get("deepseek").and_then(|v| v.as_object()))
            .map(|o| DeepSeekConfig {
                base_url:      o.get("base_url").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_DEEPSEEK_URL).to_owned(),
                api_key_env:   o.get("api_key_env").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_DEEPSEEK_KEY_ENV).to_owned(),
                api_key:       o.get("api_key").and_then(|v| v.as_str())
                                  .unwrap_or("").to_owned(),
                default_model: o.get("default_model").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_DEEPSEEK_MODEL).to_owned(),
            });

        let gemini = providers
            .and_then(|p| p.get("gemini").and_then(|v| v.as_object()))
            .map(|o| GeminiConfig {
                base_url:      o.get("base_url").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_GEMINI_URL).to_owned(),
                api_key_env:   o.get("api_key_env").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_GEMINI_KEY_ENV).to_owned(),
                api_key:       o.get("api_key").and_then(|v| v.as_str())
                                  .unwrap_or("").to_owned(),
                default_model: o.get("default_model").and_then(|v| v.as_str())
                                  .unwrap_or(DEFAULT_GEMINI_MODEL).to_owned(),
            });

        let cache = llm.get("cache").and_then(|v| v.as_object())
            .map(|c| CacheConfig {
                enabled:  c.get("enabled").and_then(|v| v.as_bool())
                            .unwrap_or(CacheConfig::default().enabled),
                ttl_secs: c.get("ttl_secs").and_then(|v| v.as_f64())
                            .map(|n| n as u64)
                            .unwrap_or(CacheConfig::default().ttl_secs),
            })
            .unwrap_or_default();

        let dedup = llm.get("dedup").and_then(|v| v.as_object())
            .map(|d| DedupConfig {
                enabled:       d.get("enabled").and_then(|v| v.as_bool())
                                 .unwrap_or(DedupConfig::default().enabled),
                window_secs:   d.get("window_secs").and_then(|v| v.as_f64())
                                 .map(|n| n as u64)
                                 .unwrap_or(DedupConfig::default().window_secs),
                wait_max_secs: d.get("wait_max_secs").and_then(|v| v.as_f64())
                                 .map(|n| n as u64)
                                 .unwrap_or(DedupConfig::default().wait_max_secs),
            })
            .unwrap_or_default();

        let chat = llm.get("chat").and_then(|v| v.as_object())
            .map(|c| {
                let bund_d = ChatBundConfig::default();
                let bund = c.get("bund").and_then(|v| v.as_object())
                    .map(|b| ChatBundConfig {
                        enabled:           b.get("enabled").and_then(|v| v.as_bool())
                                              .unwrap_or(bund_d.enabled),
                        // Clamp to [1, 60] seconds — runaway scripts
                        // wait this long before the watchdog gives up.
                        timeout_secs:      b.get("timeout_secs").and_then(|v| v.as_f64())
                                              .map(|n| n as u64)
                                              .unwrap_or(bund_d.timeout_secs)
                                              .clamp(1, 60),
                        max_result_chars:  b.get("max_result_chars").and_then(|v| v.as_f64())
                                              .map(|n| n as usize)
                                              .unwrap_or(bund_d.max_result_chars),
                        oversize_strategy: b.get("oversize_strategy").and_then(|v| v.as_str())
                                              .map(str::to_owned)
                                              .unwrap_or(bund_d.oversize_strategy.clone()),
                        slash_strictness:  b.get("slash_strictness").and_then(|v| v.as_str())
                                              .map(str::to_owned)
                                              .unwrap_or(bund_d.slash_strictness.clone()),
                        fenced_only:       b.get("fenced_only").and_then(|v| v.as_bool())
                                              .unwrap_or(bund_d.fenced_only),
                    })
                    .unwrap_or_default();
                ChatConfig { bund }
            })
            .unwrap_or_default();

        let to_bund = llm.get("to_bund").and_then(|v| v.as_object())
            .map(|t| {
                let d = ToBundConfig::default();
                ToBundConfig {
                    enabled:             t.get("enabled").and_then(|v| v.as_bool())
                                            .unwrap_or(d.enabled),
                    // Clamp to [10, 600] — translator prompts are
                    // large; sub-10s timeouts almost always fail
                    // mid-stream, sub-10-minute is plenty.
                    timeout_secs:        t.get("timeout_secs").and_then(|v| v.as_f64())
                                            .map(|n| n as u64)
                                            .unwrap_or(d.timeout_secs)
                                            .clamp(10, 600),
                    // Ceiling 5 — the model rarely converges after
                    // more than 2-3 corrective turns.
                    max_retries:         t.get("max_retries").and_then(|v| v.as_f64())
                                            .map(|n| (n as usize).min(5))
                                            .unwrap_or(d.max_retries),
                    provider:            t.get("provider").and_then(|v| v.as_str())
                                            .map(str::to_owned)
                                            .unwrap_or(d.provider.clone()),
                    model:               t.get("model").and_then(|v| v.as_str())
                                            .map(str::to_owned)
                                            .unwrap_or(d.model.clone()),
                    extra_system_prompt: t.get("extra_system_prompt").and_then(|v| v.as_str())
                                            .map(str::to_owned)
                                            .unwrap_or(d.extra_system_prompt.clone()),
                }
            })
            .unwrap_or_default();

        Self { default, ollama, anthropic, openai, deepseek, gemini, cache, dedup, chat, to_bund }
    }
}

pub struct ProviderManager {
    providers:  BTreeMap<String, Arc<dyn Provider>>,
    default_id: Option<String>,
}

impl ProviderManager {
    pub fn empty(default_id: Option<String>) -> Self {
        Self { providers: BTreeMap::new(), default_id }
    }

    pub fn insert(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        self.providers.insert(name.into(), provider);
    }

    pub fn is_empty(&self) -> bool { self.providers.is_empty() }
    pub fn len(&self)      -> usize { self.providers.len() }

    pub fn registered(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    pub fn default_id(&self) -> Option<&str> {
        self.default_id.as_deref()
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn Provider>> {
        self.providers.get(name).cloned()
            .ok_or_else(|| err_msg(format!(
                "llm: provider {name:?} not registered (have: {:?})",
                self.registered()
            )))
    }

    pub fn get_default(&self) -> Result<Arc<dyn Provider>> {
        let id = self.default_id.as_deref().ok_or_else(|| {
            err_msg(format!("llm: no default provider set (registered: {:?})", self.registered()))
        })?;
        self.get(id)
    }

    /// Convenience: `None` → default provider; `Some(name)` → named provider.
    pub fn resolve(&self, name: Option<&str>) -> Result<Arc<dyn Provider>> {
        match name {
            Some(n) => self.get(n),
            None    => self.get_default(),
        }
    }

    /// Build a manager from a parsed [`LlmConfig`].  Providers that require
    /// an env-supplied API key but find it unset are logged and skipped;
    /// the rest are registered.  Returns an empty manager (no providers,
    /// no default) when nothing was configured or every API-key check failed.
    pub fn from_config(cfg: LlmConfig) -> Self {
        let mut mgr = Self::empty(cfg.default.clone());

        if let Some(o) = cfg.ollama {
            match OllamaProvider::new(&o.url, &o.default_model) {
                Ok(p) => {
                    log::info!("[llm] registered provider 'ollama' url={} model={}",
                        o.url, o.default_model);
                    mgr.insert("ollama", Arc::new(p));
                }
                Err(e) => log::warn!("[llm] skip ollama: {e}"),
            }
        }

        if let Some(a) = cfg.anthropic {
            match std::env::var(&a.api_key_env) {
                Ok(key) if !key.is_empty() => match AnthropicProvider::new(&a.base_url, &key, &a.default_model) {
                    Ok(p) => {
                        log::info!("[llm] registered provider 'anthropic' model={} (key from ${})",
                            a.default_model, a.api_key_env);
                        mgr.insert("anthropic", Arc::new(p));
                    }
                    Err(e) => log::warn!("[llm] skip anthropic: {e}"),
                },
                _ => log::warn!("[llm] skip anthropic: env ${} unset", a.api_key_env),
            }
        }

        if let Some(o) = cfg.openai {
            match std::env::var(&o.api_key_env) {
                Ok(key) if !key.is_empty() => match OpenAIProvider::new(&o.base_url, &key, &o.default_model) {
                    Ok(p) => {
                        log::info!("[llm] registered provider 'openai' model={} (key from ${})",
                            o.default_model, o.api_key_env);
                        mgr.insert("openai", Arc::new(p));
                    }
                    Err(e) => log::warn!("[llm] skip openai: {e}"),
                },
                _ => log::warn!("[llm] skip openai: env ${} unset", o.api_key_env),
            }
        }

        // DeepSeek differs from anthropic/openai in that the key can
        // come from the env var (preferred) OR from a plaintext
        // `api_key` field directly in bds.hjson.  The env var wins
        // when both are present.  We log the source so operators can
        // tell at a glance whether the deployment is leaking the key
        // through the config file.
        if let Some(d) = cfg.deepseek {
            let (key, source) = match std::env::var(&d.api_key_env) {
                Ok(k) if !k.is_empty() => (k, format!("${}", d.api_key_env)),
                _ if !d.api_key.is_empty() => (d.api_key.clone(),
                    "bds.hjson:llm.providers.deepseek.api_key".to_owned()),
                _ => (String::new(), String::new()),
            };
            if key.is_empty() {
                log::warn!("[llm] skip deepseek: env ${} unset and no `api_key` field \
                            in bds.hjson llm.providers.deepseek", d.api_key_env);
            } else {
                match DeepSeekProvider::new(&d.base_url, &key, &d.default_model) {
                    Ok(p) => {
                        log::info!("[llm] registered provider 'deepseek' model={} (key from {})",
                            d.default_model, source);
                        mgr.insert("deepseek", Arc::new(p));
                    }
                    Err(e) => log::warn!("[llm] skip deepseek: {e}"),
                }
            }
        }

        // Gemini follows the same env-or-hjson key resolution as
        // DeepSeek.  Google API keys rotate often, so the env-var path
        // is strongly preferred; the plaintext `api_key` field exists
        // for parity but operators should treat it as a stop-gap.
        if let Some(g) = cfg.gemini {
            let (key, source) = match std::env::var(&g.api_key_env) {
                Ok(k) if !k.is_empty() => (k, format!("${}", g.api_key_env)),
                _ if !g.api_key.is_empty() => (g.api_key.clone(),
                    "bds.hjson:llm.providers.gemini.api_key".to_owned()),
                _ => (String::new(), String::new()),
            };
            if key.is_empty() {
                log::warn!("[llm] skip gemini: env ${} unset and no `api_key` field \
                            in bds.hjson llm.providers.gemini", g.api_key_env);
            } else {
                match GeminiProvider::new(&g.base_url, &key, &g.default_model) {
                    Ok(p) => {
                        log::info!("[llm] registered provider 'gemini' model={} (key from {})",
                            g.default_model, source);
                        mgr.insert("gemini", Arc::new(p));
                    }
                    Err(e) => log::warn!("[llm] skip gemini: {e}"),
                }
            }
        }

        // If the configured default wasn't actually registered, fall back to
        // the first registered name so `resolve(None)` still works.
        if let Some(id) = &mgr.default_id {
            if !mgr.providers.contains_key(id) {
                let fallback = mgr.providers.keys().next().cloned();
                if let Some(f) = &fallback {
                    log::warn!("[llm] configured default {id:?} not registered; using {f:?}");
                }
                mgr.default_id = fallback;
            }
        } else if mgr.default_id.is_none() {
            mgr.default_id = mgr.providers.keys().next().cloned();
        }
        mgr
    }

    pub fn load_from_hjson(path: &str) -> Self {
        Self::from_config(LlmConfig::load_from_hjson(path))
    }
}

static GLOBAL: OnceLock<ProviderManager> = OnceLock::new();

/// Initialise the process-wide manager.  First call wins; subsequent
/// calls are no-ops (so binaries that share startup code with tests
/// don't double-init).
pub fn init(manager: ProviderManager) {
    let _ = GLOBAL.set(manager);
}

/// Process-wide manager.  Returns `None` until [`init`] has been called.
pub fn manager() -> Option<&'static ProviderManager> {
    GLOBAL.get()
}
