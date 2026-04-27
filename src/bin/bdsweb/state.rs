use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub node_url: Arc<String>,
    pub http:     reqwest::Client,
}

impl AppState {
    pub fn new(node_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self { node_url: Arc::new(node_url), http }
    }
}
