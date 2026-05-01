use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub node_url:     Arc<String>,
    pub http:         reqwest::Client,
    /// Ollama model name read from bds.hjson (for display in the Chat UI).
    pub ollama_model: Arc<String>,
}

impl AppState {
    pub fn new(node_url: String, ollama_model: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("failed to build HTTP client");
        Self {
            node_url:     Arc::new(node_url),
            http,
            ollama_model: Arc::new(ollama_model),
        }
    }
}
