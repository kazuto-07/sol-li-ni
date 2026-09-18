//! The language model: OpenAI chat completions, streamed.
//!
//! OpenAI streams over SSE rather than a WebSocket (only its Realtime API uses one, and that
//! would replace Deepgram and Murf too). Tokens are yielded as they arrive so speech can start
//! before the sentence is finished.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Groq by default: OpenAI-compatible, free to start with, and quick enough to keep the first
/// token inside the latency budget. `OPENAI_BASE_URL` points this at any other such API.
pub const DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// A warmup is an optimisation, never a reason to hold up a call.
const WARM_TIMEOUT: Duration = Duration::from_secs(5);

/// The shared HTTP client: one connection pool for the whole process.
pub fn client() -> reqwest::Client {
    // reqwest's `rustls-no-provider` build panics without this.
    crate::tls::install();
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("building an HTTP client cannot fail with these settings")
}

/// Short answers, spoken aloud: no markdown, no lists, no emoji.
pub const SYSTEM_PROMPT: &str = "You are a voice assistant in a live phone-style conversation. \
Reply in one or two short sentences. Write plain spoken language: no markdown, no bullet \
points, no emoji, no stage directions. If you need something clarified, just ask.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".to_owned(), content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".to_owned(), content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".to_owned(), content: content.into() }
    }
}

/// Cheap to clone: the client shares one connection pool.
#[derive(Clone)]
pub struct Llm {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    /// Left out of the request when `None`: some models only accept their own default, and
    /// sending one anyway is an error.
    temperature: Option<f32>,
}

impl Llm {
    /// `base_url` is an OpenAI-compatible root such as `https://api.openai.com/v1`. The client
    /// is shared so its connection pool is reused across calls.
    pub fn new(client: reqwest::Client, base_url: &str, api_key: String, model: String) -> Self {
        Self {
            client,
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key,
            model,
            temperature: None,
        }
    }

    /// `None` keeps the model's own default.
    pub fn with_temperature(mut self, temperature: Option<f32>) -> Self {
        self.temperature = temperature;
        self
    }

    /// Opens the connection before it is needed, so the first reply of a call does not pay for
    /// DNS, TLS and HTTP/2 setup. The response is irrelevant — only the connection matters, and
    /// it stays in the pool for the request that follows.
    pub async fn warm(&self) {
        let started = std::time::Instant::now();
        match self.client.get(&self.url).timeout(WARM_TIMEOUT).send().await {
            Ok(_) => info!("llm warm in {} ms ({})", started.elapsed().as_millis(), self.url),
            Err(e) => debug!("llm warmup failed, the first reply will be slower: {e}"),
        }
    }

    /// Streams the reply to `messages` token by token.
    pub async fn stream(&self, messages: &[Message]) -> Result<impl Stream<Item = Result<String>>> {
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&Request {
                model: &self.model,
                messages,
                stream: true,
                temperature: self.temperature,
            })
            .send()
            .await
            .context("OpenAI request failed")?;

        // Surface the API's own message: a wrong model name or an unfunded key says so here.
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("OpenAI returned {status}: {body}");
        }

        Ok(response.bytes_stream().eventsource().filter_map(|event| async move {
            match event {
                Ok(event) if event.data == "[DONE]" => None,
                Ok(event) => match serde_json::from_str::<Chunk>(&event.data) {
                    Ok(chunk) => chunk.token().map(Ok),
                    Err(e) => {
                        debug!("unparsed openai chunk ({e}): {}", event.data);
                        None
                    }
                },
                Err(e) => Some(Err(anyhow!("OpenAI stream failed: {e}"))),
            }
        }))
    }
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
}

impl Chunk {
    fn token(self) -> Option<String> {
        self.choices
            .into_iter()
            .next()
            .and_then(|choice| choice.delta.content)
            .filter(|token| !token.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves TLS works against the real API: with two rustls providers compiled in, this
    /// panics instead of returning an error. Network test, so not part of the default run:
    ///   cargo test -- --ignored
    #[tokio::test]
    #[ignore = "requires network access"]
    async fn tls_to_the_real_api_works() {
        let llm = Llm::new(
            client(),
            DEFAULT_BASE_URL,
            "not-a-real-key".to_owned(),
            "gpt-4o-mini".to_owned(),
        );
        let Err(error) = llm.stream(&[Message::user("hi")]).await else {
            panic!("a fake key must be rejected");
        };
        let error = error.to_string();
        assert!(error.contains("401"), "expected an auth failure, got: {error}");
    }

    /// Measures what the warmup buys: the same request, with and without a warmed pool.
    /// Network test: cargo test -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires network access"]
    async fn warming_up_shortens_the_first_request() {
        async fn first_request_ms(warm: bool) -> u128 {
            let llm = Llm::new(
                client(),
                DEFAULT_BASE_URL,
                "not-a-real-key".to_owned(),
                "gpt-4o-mini".to_owned(),
            );
            if warm {
                llm.warm().await;
            }
            let started = std::time::Instant::now();
            let _ = llm.stream(&[Message::user("hi")]).await;
            started.elapsed().as_millis()
        }

        let cold = first_request_ms(false).await;
        let warmed = first_request_ms(true).await;
        println!("first request: {cold} ms cold, {warmed} ms warmed");
        assert!(warmed < cold, "warming should not make the first request slower");
    }

    #[test]
    fn the_completions_path_is_appended_to_the_base_url() {
        let endpoint = |base| Llm::new(client(), base, "k".to_owned(), "m".to_owned()).url;
        assert_eq!(endpoint("https://api.openai.com/v1"), "https://api.openai.com/v1/chat/completions");
        // A trailing slash is a natural thing to paste; it must not double up.
        assert_eq!(endpoint("http://localhost:11434/v1/"), "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn a_chunk_yields_its_token() {
        let chunk: Chunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"Hello"},"index":0}]}"#,
        )
        .unwrap();
        assert_eq!(chunk.token().as_deref(), Some("Hello"));
    }

    #[test]
    fn chunks_without_content_are_skipped() {
        // The first chunk of a reply carries only the role, and the last carries a finish reason.
        for json in [
            r#"{"choices":[{"delta":{"role":"assistant"},"index":0}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop","index":0}]}"#,
            r#"{"choices":[]}"#,
        ] {
            let chunk: Chunk = serde_json::from_str(json).unwrap();
            assert!(chunk.token().is_none(), "{json}");
        }
    }
}
