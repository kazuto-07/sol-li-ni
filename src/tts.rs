//! Text to speech: Murf streaming WebSocket.
//!
//! Sentences go up as they are produced by the language model; audio comes back as base64
//! chunks, which are sliced into the 20 ms frames the WebRTC pacer expects. Murf can render at
//! 48 kHz, so nothing is resampled.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{debug, info, warn};

use crate::audio::{FRAME_SAMPLES, Frame, SAMPLE_RATE, Speaker};

pub const DEFAULT_URL: &str = "wss://global.api.murf.ai/v1/speech/stream-input";
pub const DEFAULT_VOICE: &str = "en-US-natalie";

/// A sentence to speak, and the turn it belongs to.
#[derive(Debug, Clone)]
pub struct Speech {
    pub text: String,
    /// Murf tracks a turn by context; a new turn gets a new id.
    pub context_id: String,
    /// Last sentence of this turn.
    pub end: bool,
}

/// What the agent asks of the voice.
#[derive(Debug, Clone)]
pub enum Command {
    Speak(Speech),
    /// The caller interrupted: stop this turn. Murf cancels anything not yet synthesised, and
    /// audio already in flight is ignored until the next turn starts.
    Clear { context_id: String },
}

/// A Murf socket that is connected and waiting for text.
pub struct Session {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    voice: String,
}

/// The voice configuration message, optionally initialising a context with it.
fn voice_config(voice: &str, context_id: Option<&str>) -> serde_json::Value {
    let mut message = json!({
        "voice_config": {
            "voiceId": voice,
            "style": "Conversational",
            "rate": 0,
            "pitch": 0,
            "variation": 1,
        }
    });
    if let Some(context_id) = context_id {
        message["context_id"] = json!(context_id);
    }
    message
}

/// Opens the socket and sends the voice configuration, so only text remains to be sent.
pub async fn connect(url: &str, api_key: &str, voice: &str) -> Result<Session> {
    let separator = if url.contains('?') { '&' } else { '?' };
    let url = format!(
        "{url}{separator}api-key={api_key}&sample_rate={SAMPLE_RATE}&channel_type=MONO&format=PCM"
    );

    let started = std::time::Instant::now();
    let (mut socket, response) = connect_async(url.into_client_request()?)
        .await
        .context("Murf connect failed (check MURF_API_KEY)")?;
    debug!("murf handshake: {}", response.status());

    // Sets the connection default. Each context is configured again as it starts, because a
    // context created by a text message alone falls back to Murf's default voice.
    socket.send(Message::Text(voice_config(voice, None).to_string().into())).await?;
    info!("murf warm in {} ms (voice {voice})", started.elapsed().as_millis());

    Ok(Session { socket, voice: voice.to_owned() })
}

impl Session {
    /// Speaks every sentence it is given, pushing 20 ms frames to `speaker`.
    pub async fn run(
        self,
        mut commands: mpsc::Receiver<Command>,
        speaker: Arc<Speaker>,
    ) -> Result<()> {
        let (mut tx, mut rx) = self.socket.split();
        let voice = self.voice;
        // Set while a turn is cancelled, so audio still arriving for it is discarded.
        let interrupted = Arc::new(AtomicBool::new(false));

        let uplink = {
            let interrupted = Arc::clone(&interrupted);
            async move {
            let mut configured: Option<String> = None;
            while let Some(command) = commands.recv().await {
                let speech = match command {
                    Command::Speak(speech) => {
                        interrupted.store(false, Ordering::Relaxed);
                        speech
                    }
                    Command::Clear { context_id } => {
                        interrupted.store(true, Ordering::Relaxed);
                        let message = json!({ "context_id": context_id, "clear": true });
                        tx.send(Message::Text(message.to_string().into())).await?;
                        continue;
                    }
                };
                // A new turn means a new context, which needs the voice set before any text.
                if configured.as_deref() != Some(speech.context_id.as_str()) {
                    let config = voice_config(&voice, Some(&speech.context_id));
                    tx.send(Message::Text(config.to_string().into())).await?;
                    configured = Some(speech.context_id.clone());
                }

                let message = json!({
                    "text": speech.text,
                    "context_id": speech.context_id,
                    "end": speech.end,
                });
                tx.send(Message::Text(message.to_string().into())).await?;
            }
            Ok::<_, anyhow::Error>(())
            }
        };

        let downlink = async move {
            let mut pcm = PcmFrames::default();
            while let Some(message) = rx.next().await {
                let Message::Text(text) = message? else { continue };
                let response: Value =
                    serde_json::from_str(&text).context("murf sent invalid JSON")?;

                if let Some(chunk) = response["audio"].as_str() {
                    let bytes = BASE64.decode(chunk).context("murf sent invalid base64")?;
                    let frames = pcm.push(&bytes);
                    // Audio synthesised before the interruption reached Murf is not wanted.
                    if interrupted.load(Ordering::Relaxed) {
                        continue;
                    }
                    for frame in frames {
                        speaker.push(frame);
                    }
                } else if let Some(error) = response["error"].as_str() {
                    bail!("murf: {error}");
                } else if let Some(warning) = response["warning"].as_str() {
                    warn!("murf: {warning}");
                } else {
                    debug!("murf message: {text}");
                }
            }
            Ok(())
        };

        tokio::try_join!(uplink, downlink).map(|_| ())
    }
}

/// Turns Murf's arbitrarily sized PCM chunks into whole 20 ms frames.
#[derive(Default)]
struct PcmFrames {
    /// Bytes left over from the previous chunk.
    remainder: Vec<u8>,
    header_checked: bool,
}

impl PcmFrames {
    fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        let mut bytes = chunk;

        // The first chunk can carry a 44-byte WAV header even when PCM was requested.
        if !self.header_checked {
            self.header_checked = true;
            if bytes.starts_with(b"RIFF") && bytes.len() >= 44 {
                bytes = &bytes[44..];
            }
        }

        self.remainder.extend_from_slice(bytes);
        let frame_bytes = FRAME_SAMPLES * 2;
        let whole = self.remainder.len() / frame_bytes;
        let consumed = whole * frame_bytes;

        let frames = self.remainder[..consumed]
            .chunks_exact(frame_bytes)
            .map(|frame| {
                frame
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&sample| i16::from_le_bytes(sample) as f32 / 32768.0)
                    .collect()
            })
            .collect();
        self.remainder.drain(..consumed);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: usize) -> Vec<u8> {
        (0..samples).flat_map(|i| (i as i16).to_le_bytes()).collect()
    }

    #[test]
    fn chunks_are_reassembled_into_whole_frames() {
        let mut frames = PcmFrames::default();
        // Half a frame yields nothing yet.
        assert!(frames.push(&pcm(FRAME_SAMPLES / 2)).is_empty());
        // The other half completes it, and the spare samples are held back.
        let out = frames.push(&pcm(FRAME_SAMPLES / 2 + 10));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), FRAME_SAMPLES);
        assert_eq!(frames.remainder.len(), 20);
    }

    #[test]
    fn a_wav_header_on_the_first_chunk_is_skipped() {
        let mut header = b"RIFF".to_vec();
        header.extend(std::iter::repeat_n(0u8, 40));
        header.extend(pcm(FRAME_SAMPLES));

        let mut frames = PcmFrames::default();
        let out = frames.push(&header);
        assert_eq!(out.len(), 1, "the header must not eat into the audio");
        assert_eq!(out[0][0], 0.0);
        assert_eq!(out[0][1], 1.0 / 32768.0);
    }

    #[test]
    fn raw_pcm_without_a_header_is_left_alone() {
        let mut frames = PcmFrames::default();
        let out = frames.push(&pcm(FRAME_SAMPLES));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][1], 1.0 / 32768.0);
    }
}
