//! A stand-in for Deepgram's streaming API, for testing the pipeline without an API key.
//!
//! Run it, then point the server at it:
//!   cargo run --example mock_deepgram
//!   DEEPGRAM_URL=ws://127.0.0.1:9999/v1/listen DEEPGRAM_API_KEY=x cargo run
//!
//! It accepts audio and, using the arriving audio as a clock, emits a first turn and then —
//! while the agent is still replying — a second one, so barge-in can be exercised.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// One second of 48 kHz 16-bit mono audio.
const SECOND: usize = 48_000 * 2;
/// The caller's first turn, then an interruption while the agent is talking back.
const FIRST_TURN: usize = SECOND;
const BARGE_IN: usize = 3 * SECOND;

#[tokio::main]
async fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:9999").await?;
    println!("mock deepgram on ws://127.0.0.1:9999/v1/listen");

    loop {
        let (stream, addr) = listener.accept().await?;
        tokio::spawn(async move {
            let mut socket = match tokio_tungstenite::accept_async(stream).await {
                Ok(s) => s,
                Err(e) => return println!("handshake failed: {e}"),
            };
            println!("client connected: {addr}");

            let mut received = 0usize;
            let mut turns_sent = 0usize;
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Binary(audio) => received += audio.len(),
                    Message::Text(text) => println!("control: {text}"),
                    Message::Close(_) => break,
                    _ => continue,
                }
                let turn: &[&str] = if received >= FIRST_TURN && turns_sent == 0 {
                    turns_sent = 1;
                    &[
                        r#"{"type":"SpeechStarted","channel":[0],"timestamp":0.0}"#,
                        r#"{"type":"Results","is_final":false,"speech_final":false,"start":0.0,"duration":0.6,"channel":{"alternatives":[{"transcript":"hello from the"}]}}"#,
                        r#"{"type":"Results","is_final":true,"speech_final":true,"start":0.0,"duration":0.95,"channel":{"alternatives":[{"transcript":"hello from the mock"}]}}"#,
                        r#"{"type":"UtteranceEnd","channel":[0],"last_word_end":1.0}"#,
                    ]
                } else if received >= BARGE_IN && turns_sent == 1 {
                    turns_sent = 2;
                    println!("--- barge-in: caller talks over the agent");
                    &[
                        r#"{"type":"SpeechStarted","channel":[0],"timestamp":2.0}"#,
                        r#"{"type":"Results","is_final":false,"speech_final":false,"start":2.0,"duration":0.4,"channel":{"alternatives":[{"transcript":"actually wait"}]}}"#,
                        r#"{"type":"Results","is_final":true,"speech_final":true,"start":2.0,"duration":0.8,"channel":{"alternatives":[{"transcript":"actually wait stop"}]}}"#,
                        r#"{"type":"UtteranceEnd","channel":[0],"last_word_end":2.8}"#,
                    ]
                } else {
                    continue;
                };

                // Pretend the service needs a moment, so the measured lag is non-zero.
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                for message in turn {
                    if socket.send(Message::Text((*message).into())).await.is_err() {
                        return;
                    }
                }
            }
            println!("client gone after {received} bytes of audio");
        });
    }
}
