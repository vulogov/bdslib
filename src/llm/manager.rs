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
    AnthropicProvider, OllamaProvider, OpenAIProvider, Provider,
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

#[derive(Debug, Clone)]
pub struct OllamaConfig    { pub url: String,      pub default_model: String }

#[derive(Debug, Clone)]
pub struct AnthropicConfig { pub base_url: String, pub api_key_env: String, pub default_model: String }

#[derive(Debug, Clone)]
pub struct OpenAIConfig    { pub base_url: String, pub api_key_env: String, pub default_model: String }

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

#[derive(Debug, Clone, Default)]
pub struct LlmConfig {
    pub default:   Option<String>,
    pub ollama:    Option<OllamaConfig>,
    pub anthropic: Option<AnthropicConfig>,
    pub openai:    Option<OpenAIConfig>,
    pub cache:     CacheConfig,
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

        let cache = llm.get("cache").and_then(|v| v.as_object())
            .map(|c| CacheConfig {
                enabled:  c.get("enabled").and_then(|v| v.as_bool())
                            .unwrap_or(CacheConfig::default().enabled),
                ttl_secs: c.get("ttl_secs").and_then(|v| v.as_f64())
                            .map(|n| n as u64)
                            .unwrap_or(CacheConfig::default().ttl_secs),
            })
            .unwrap_or_default();

        Self { default, ollama, anthropic, openai, cache }
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
