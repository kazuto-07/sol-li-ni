//! A stand-in for OpenAI's streaming chat completions, for testing without an API key.
//!
//!   cargo run --example mock_openai
//!   OPENAI_URL=http://127.0.0.1:9998/v1/chat/completions OPENAI_API_KEY=x cargo run
//!
//! It echoes back what it was asked and streams the reply token by token, after a short
//! "thinking" delay so first-token latency is visible in the metrics.

use std::time::Duration;

use anyhow::Result;
use axum::response::sse::{Event, Sse};
use axum::{Json, Router, routing::post};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

const FIRST_TOKEN_DELAY: Duration = Duration::from_millis(400);
const TOKEN_INTERVAL: Duration = Duration::from_millis(40);

#[tokio::main]
async fn main() -> Result<()> {
    let app = Router::new().route("/v1/chat/completions", post(completions));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:9998").await?;
    println!("mock openai on http://127.0.0.1:9998/v1/chat/completions");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn completions(Json(body): Json<Value>) -> Sse<impl stream::Stream<Item = Result<Event>>> {
    let asked = body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_str())
        .unwrap_or("nothing")
        .to_owned();
    println!("asked: {asked}");

    let reply = format!("You said: {asked} That is all I can say for now.");
    let tokens: Vec<String> = reply.split_inclusive(' ').map(str::to_owned).collect();

    let stream = stream::unfold(tokens.into_iter().enumerate(), |mut tokens| async move {
        let (index, token) = tokens.next()?;
        tokio::time::sleep(if index == 0 { FIRST_TOKEN_DELAY } else { TOKEN_INTERVAL }).await;
        let chunk = json!({ "choices": [{ "index": 0, "delta": { "content": token } }] });
        Some((Ok(Event::default().data(chunk.to_string())), tokens))
    })
    .chain(stream::once(async { Ok(Event::default().data("[DONE]")) }));

    Sse::new(stream)
}
