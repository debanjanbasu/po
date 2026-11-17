//! Chat providers: where completions come from.
//!
//! po talks to models through one trait. Two implementations ship today:
//! [`MockEchoProvider`] (a deterministic echo used for demos and tests) and
//! [`NanoServeProvider`] (any OpenAI-compatible HTTP endpoint, including
//! `nano-ai serve` on localhost). The in-process nano-engine adapter lives in
//! [`crate::nanoai`] behind the `local-engine` feature.

use tokio::sync::mpsc::UnboundedSender;

use crate::model::ChatMessage;

/// Errors raised while talking to a provider.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The request could not be delivered.
    #[error("http error: {0}")]
    Http(String),
    /// The endpoint answered, but not with a valid completion.
    #[error("bad response: {0}")]
    BadResponse(String),
    /// The provider configuration is invalid (e.g. https without TLS support).
    #[error("configuration error: {0}")]
    Config(String),
    /// The provider was asked for something it cannot do.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// A source of chat completions.
///
/// Implementations stream pieces through `pieces` (mirroring
/// [`StreamEvent::Delta`]) and return the full text when the turn ends.
pub trait ChatProvider: Send + Sync {
    /// Human-readable provider name for the status bar.
    fn label(&self) -> String;

    /// Run one turn: send `messages`, stream pieces into `pieces`, and
    /// resolve with the completed reply.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when the turn cannot complete.
    fn complete(
        &self,
        messages: &[ChatMessage],
        pieces: Option<UnboundedSender<String>>,
    ) -> impl Future<Output = Result<String, ProviderError>> + Send;
}

/// A deterministic provider that echoes the user's message back in chunks.
///
/// This keeps the whole chat flow demonstrable — submit, streaming deltas,
/// turn completion — with no model present, and it makes the harness
/// testable without a network.
#[derive(Debug, Clone, Default)]
pub struct MockEchoProvider {
    /// Optional prefix so tests can identify the reply source.
    pub prefix: String,
}

impl MockEchoProvider {
    /// Create an echo provider with a reply prefix.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self { prefix: prefix.into() }
    }

    /// The reply this provider would produce for `messages`.
    #[must_use]
    pub fn reply_for(&self, messages: &[ChatMessage]) -> String {
        let last = messages
            .iter()
            .rev()
            .find(|m| m.role == crate::model::ChatRole::User);
        let text = last.map_or("…", |m| m.content.as_str());
        format!("{}echo: {}", self.prefix, text.trim())
    }
}

impl ChatProvider for MockEchoProvider {
    fn label(&self) -> String {
        "mock (echo)".into()
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        pieces: Option<UnboundedSender<String>>,
    ) -> Result<String, ProviderError> {
        let reply = self.reply_for(messages);
        // Stream word-by-word so the UI's incremental path is exercised.
        for word in reply.split_inclusive(char::is_whitespace) {
            if let Some(tx) = &pieces {
                let _ = tx.send(word.to_string());
            }
            tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        }
        Ok(reply)
    }
}

/// An OpenAI-compatible HTTP endpoint (`/v1/chat/completions`).
///
/// `nano-ai serve` speaks this protocol on localhost, as do the hosted
/// providers; one client covers all of them. Requests run on the blocking
/// thread pool so callers can stay `async`.
#[derive(Debug, Clone)]
pub struct NanoServeProvider {
    /// Base URL, e.g. `http://127.0.0.1:8080`.
    pub base_url: String,
    /// Bearer token, if the endpoint needs one.
    pub api_key: Option<String>,
    /// Model name passed through to the endpoint.
    pub model: String,
}

impl NanoServeProvider {
    /// Create a client for `base_url`.
    #[must_use]
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
        }
    }

    /// The full chat-completions endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/v1/chat/completions")
    }

    /// Serialize the request body for `messages`.
    #[must_use]
    pub fn request_body(&self, messages: &[ChatMessage]) -> String {
        let body = serde_json::json!({
            "model": self.model,
            "stream": true,
            "messages": messages.iter().map(|m| serde_json::json!({
                "role": m.role.role_label(),
                "content": m.content,
            })).collect::<Vec<_>>(),
        });
        body.to_string()
    }
}

impl ChatProvider for NanoServeProvider {
    fn label(&self) -> String {
        format!("serve: {}", self.model)
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        pieces: Option<UnboundedSender<String>>,
    ) -> Result<String, ProviderError> {
        if !self.base_url.starts_with("http://") {
            // std's TcpStream client only speaks plaintext http; https needs a
            // TLS stack, which po deliberately does not vendor.
            return Err(ProviderError::Config(
                "https endpoints need a TLS-capable client; use http:// (nano-ai serve) or the local engine".into(),
            ));
        }

        let body = self.request_body(messages);
        let endpoint = self.endpoint();
        let api_key = self.api_key.clone();
        let stream = tokio::task::spawn_blocking(move || {
            post_sse(&endpoint, api_key.as_deref(), &body, pieces.as_ref())
        })
        .await
        .map_err(|e| ProviderError::Http(e.to_string()))??;
        Ok(stream)
    }
}

/// Blocking HTTP POST that consumes an SSE stream, forwarding `content`
/// deltas through `pieces` and returning the concatenated reply.
fn post_sse(
    endpoint: &str,
    api_key: Option<&str>,
    body: &str,
    pieces: Option<&UnboundedSender<String>>,
) -> Result<String, ProviderError> {
    use std::io::{BufRead, BufReader, Write};

    let url = url_parts(endpoint)?;
    let host = url.host.clone();
    let stream = std::net::TcpStream::connect((host.clone(), url.port))
        .map_err(|e| ProviderError::Http(format!("connect {host}: {e}")))?;
    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        url.path,
        url.host_header(),
        body.len()
    );
    if let Some(key) = api_key {
        req.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    req.push_str("\r\n");
    let mut sock = std::io::BufWriter::new(&stream);
    sock.write_all(req.as_bytes())
        .map_err(|e| ProviderError::Http(e.to_string()))?;
    sock.write_all(body.as_bytes())
        .map_err(|e| ProviderError::Http(e.to_string()))?;
    sock.flush().map_err(|e| ProviderError::Http(e.to_string()))?;
    drop(sock);

    let reader = BufReader::new(stream);
    let mut reply = String::new();
    let mut status_seen = false;
    for line in reader.lines() {
        let line = line.map_err(|e| ProviderError::Http(e.to_string()))?;
        if !status_seen {
            if !line.starts_with("HTTP/1.1 200") && !line.starts_with("HTTP/1.0 200") {
                return Err(ProviderError::BadResponse(format!(
                    "unexpected status: {line}"
                )));
            }
            status_seen = true;
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let Some(delta) = value
            .pointer("/choices/0/delta/content")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        reply.push_str(delta);
        if let Some(tx) = pieces {
            let _ = tx.send(delta.to_string());
        }
    }
    if !status_seen {
        return Err(ProviderError::BadResponse("no status line".into()));
    }
    Ok(reply)
}

/// The pieces of a minimal http:// URL.
struct UrlParts {
    host: String,
    port: u16,
    path: String,
}

impl UrlParts {
    /// The value for the HTTP `Host` header (host[:port] when non-default).
    fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Parse `http://host[:port]/path`.
fn url_parts(endpoint: &str) -> Result<UrlParts, ProviderError> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| ProviderError::Config(format!("only http:// is supported: {endpoint}")))?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err(ProviderError::Config(format!("no host in {endpoint}")));
    }
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| ProviderError::Config(format!("bad port in {endpoint}")))?,
        ),
        None => (authority.to_string(), 80),
    };
    Ok(UrlParts { host, port, path })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    #[tokio::test]
    async fn echo_provider_streams_the_last_user_message() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let provider = MockEchoProvider::new("[demo] ");
        let reply = provider
            .complete(&[user("hello world")], Some(tx))
            .await
            .expect("complete");
        assert_eq!(reply, "[demo] echo: hello world");

        let mut streamed = String::new();
        while let Ok(piece) = rx.try_recv() {
            streamed.push_str(&piece);
        }
        assert_eq!(streamed, reply, "pieces must reassemble the reply");
    }

    #[tokio::test]
    async fn https_endpoints_are_rejected_with_a_config_error() {
        let provider = NanoServeProvider::new("https://api.example.com", "m");
        let error = provider
            .complete(&[user("hi")], None)
            .await
            .expect_err("https must be refused");
        assert!(matches!(error, ProviderError::Config(_)));
    }

    #[test]
    fn endpoint_and_body_are_openai_shaped() {
        let provider = NanoServeProvider::new("http://127.0.0.1:8080/", "minicpm");
        assert_eq!(provider.endpoint(), "http://127.0.0.1:8080/v1/chat/completions");
        let body: serde_json::Value =
            serde_json::from_str(&provider.request_body(&[user("hi")])).expect("json");
        assert_eq!(body["model"], "minicpm");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn url_parsing_covers_host_port_and_path() {
        let parts = url_parts("http://localhost:8080/v1/x").expect("parsed");
        assert_eq!(parts.host, "localhost");
        assert_eq!(parts.port, 8080);
        assert_eq!(parts.path, "/v1/x");
        assert_eq!(parts.host_header(), "localhost:8080");

        let plain = url_parts("http://example.com").expect("parsed");
        assert_eq!(plain.port, 80);
        assert_eq!(plain.host_header(), "example.com");

        assert!(url_parts("https://example.com").is_err());
    }
}
