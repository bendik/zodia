//! Minimal client for a locally-running Ollama instance
//! (<https://ollama.com>) — powers an optional "generate a draft with
//! your local AI" affordance. Entirely opportunistic: if Ollama isn't
//! installed or running, [`OllamaClient::detect_models`] returns `None`
//! and the app doesn't offer the feature at all. Never required for
//! anything — no network call here is on any path the app needs to work.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Ollama's default local API — always plain HTTP, never TLS, since it
/// only ever listens on localhost.
pub const DEFAULT_HOST: &str = "http://localhost:11434";

const DETECT_TIMEOUT: Duration = Duration::from_secs(2);
const GENERATE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct OllamaClient {
    host:   String,
    client: reqwest::Client,
}

impl Default for OllamaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaClient {
    pub fn new() -> Self {
        Self::with_host(DEFAULT_HOST)
    }

    pub fn with_host(host: impl Into<String>) -> Self {
        Self { host: host.into(), client: reqwest::Client::new() }
    }

    /// Probe whether Ollama is reachable and list installed model names.
    /// `None` covers every failure mode uniformly (not installed, not
    /// running, wrong port, malformed response) — this is a "maybe
    /// available" check the caller doesn't need to distinguish further:
    /// the magic button simply doesn't appear.
    pub async fn detect_models(&self) -> Option<Vec<String>> {
        let resp = self.client.get(format!("{}/api/tags", self.host))
            .timeout(DETECT_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let tags: TagsResponse = resp.json().await.ok()?;
        Some(tags.models.into_iter().map(|m| m.name).collect())
    }

    /// Generate a completion for `prompt` with `model`. Non-streaming —
    /// the response is one short interpretation, not worth the
    /// complexity of consuming a stream for.
    pub async fn generate(&self, model: &str, prompt: &str) -> Result<String, OllamaError> {
        let req = GenerateRequest {
            model:  model.to_string(),
            prompt: prompt.to_string(),
            stream: false,
        };
        let resp = self.client.post(format!("{}/api/generate", self.host))
            .json(&req)
            .timeout(GENERATE_TIMEOUT)
            .send()
            .await
            .map_err(|e| OllamaError::Request(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(OllamaError::Status(resp.status().as_u16()));
        }
        let body: GenerateResponse = resp.json().await
            .map_err(|e| OllamaError::Decode(e.to_string()))?;
        Ok(body.response)
    }
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Vec<TagModel>,
}

#[derive(Debug, Deserialize)]
struct TagModel {
    name: String,
}

#[derive(Debug, Serialize)]
struct GenerateRequest {
    model:  String,
    prompt: String,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct GenerateResponse {
    response: String,
}

#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error("request to Ollama failed: {0}")]
    Request(String),
    #[error("Ollama returned HTTP {0}")]
    Status(u16),
    #[error("failed to decode Ollama's response: {0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Bind an ephemeral local port, accept exactly one connection, read
    /// (and discard) its request, then write back `raw_response` verbatim.
    /// Returns the `http://127.0.0.1:PORT` base url to point an
    /// `OllamaClient` at. No mock-server framework — Ollama's API surface
    /// used here is two endpoints and canned JSON, not worth the dependency.
    async fn serve_once_raw(raw_response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(raw_response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
        format!("http://127.0.0.1:{port}")
    }

    async fn serve_once_json(body: String) -> String {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body,
        );
        serve_once_raw(Box::leak(response.into_boxed_str())).await
    }

    #[tokio::test]
    async fn detect_models_lists_installed_models() {
        let host = serve_once_json(
            r#"{"models":[{"name":"llama3.2"},{"name":"mistral"}]}"#.to_string(),
        ).await;
        let client = OllamaClient::with_host(host);
        let models = client.detect_models().await;
        assert_eq!(models, Some(vec!["llama3.2".to_string(), "mistral".to_string()]));
    }

    #[tokio::test]
    async fn detect_models_returns_none_when_nothing_is_listening() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // free the port immediately; nothing listens on it now
        let client = OllamaClient::with_host(format!("http://127.0.0.1:{port}"));
        assert_eq!(client.detect_models().await, None);
    }

    #[tokio::test]
    async fn detect_models_returns_none_for_a_non_success_status() {
        let host = serve_once_raw(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ).await;
        let client = OllamaClient::with_host(host);
        assert_eq!(client.detect_models().await, None);
    }

    #[tokio::test]
    async fn detect_models_returns_none_for_malformed_json() {
        let host = serve_once_json("not json".to_string()).await;
        let client = OllamaClient::with_host(host);
        assert_eq!(client.detect_models().await, None);
    }

    #[tokio::test]
    async fn generate_returns_the_response_text() {
        let host = serve_once_json(
            r#"{"model":"llama3.2","response":"The will meets the feelings with ease.","done":true}"#.to_string(),
        ).await;
        let client = OllamaClient::with_host(host);
        let text = client.generate("llama3.2", "some prompt").await.unwrap();
        assert_eq!(text, "The will meets the feelings with ease.");
    }

    #[tokio::test]
    async fn generate_surfaces_a_non_success_status_as_an_error() {
        let host = serve_once_raw(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ).await;
        let client = OllamaClient::with_host(host);
        let err = client.generate("llama3.2", "x").await.unwrap_err();
        assert!(matches!(err, OllamaError::Status(404)));
    }
}
