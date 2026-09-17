//! A stand-in for Murf's streaming TTS, for testing without an API key.
//!
//!   cargo run --example mock_murf
//!   MURF_URL=ws://127.0.0.1:9997/v1/speech/stream-input MURF_API_KEY=x cargo run
//!
//! It answers each sentence with a tone whose length matches the text, base64-encoded 48 kHz
//! PCM, prefixed with a WAV header on the first chunk exactly as Murf does.

use std::f32::consts::TAU;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const SAMPLE_RATE: usize = 48_000;
/// Roughly a spoken syllable per two characters.
const MS_PER_CHAR: usize = 60;
const TONE_HZ: f32 = 220.0;
/// Murf answers in chunks rather than one blob.
const CHUNK_SAMPLES: usize = 4_800;

#[tokio::main]
async fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:9997").await?;
    println!("mock murf on ws://127.0.0.1:9997/v1/speech/stream-input");

    loop {
        let (stream, addr) = listener.accept().await?;
        tokio::spawn(async move {
            let mut socket = match tokio_tungstenite::accept_async(stream).await {
                Ok(socket) => socket,
                Err(e) => return println!("handshake failed: {e}"),
            };
            println!("client connected: {addr}");
            let mut first_chunk = true;

            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else { continue };
                let Ok(request) = serde_json::from_str::<Value>(&text) else { continue };

                if request.get("voice_config").is_some() {
                    // Murf warns NO_VOICE_CONFIG if a context gets text before a config.
                    match request["context_id"].as_str() {
                        Some(context) => println!("voice config for context {context:?}"),
                        None => println!("voice config for the connection"),
                    }
                    continue;
                }

                if request["clear"].as_bool().unwrap_or(false) {
                    let context = request["context_id"].as_str().unwrap_or("?");
                    println!("CLEAR context {context:?} — synthesis cancelled");
                    continue;
                }

                let spoken = request["text"].as_str().unwrap_or_default();
                let end = request["end"].as_bool().unwrap_or(false);
                println!("speak {spoken:?} (end: {end})");
                if spoken.is_empty() {
                    continue;
                }

                let samples = spoken.len() * MS_PER_CHAR * SAMPLE_RATE / 1000;
                for (index, chunk) in tone(samples).chunks(CHUNK_SAMPLES * 2).enumerate() {
                    let mut payload = Vec::new();
                    if first_chunk {
                        first_chunk = false;
                        payload.extend_from_slice(&wav_header());
                    }
                    payload.extend_from_slice(chunk);
                    let message = json!({
                        "audio": BASE64.encode(&payload),
                        "final": index == 0 && end,
                    });
                    if socket.send(Message::Text(message.to_string().into())).await.is_err() {
                        return;
                    }
                }
            }
            println!("client gone");
        });
    }
}

/// A quiet sine wave, as little-endian 16-bit PCM.
fn tone(samples: usize) -> Vec<u8> {
    (0..samples)
        .flat_map(|i| {
            let value = (TAU * TONE_HZ * i as f32 / SAMPLE_RATE as f32).sin() * 0.25;
            ((value * i16::MAX as f32) as i16).to_le_bytes()
        })
        .collect()
}

/// A 44-byte RIFF header, which Murf puts on the first chunk.
fn wav_header() -> [u8; 44] {
    let mut header = [0u8; 44];
    header[..4].copy_from_slice(b"RIFF");
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[36..40].copy_from_slice(b"data");
    header
}
