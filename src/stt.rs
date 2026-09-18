//! Speech to text: Deepgram streaming WebSocket.
//!
//! Deepgram accepts 48 kHz linear16 directly, so caller audio goes up as-is with no
//! resampling. `endpointing` + `utterance_end_ms` tell us when the caller stopped talking.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, info, warn};

use crate::audio::{Frame, SAMPLE_RATE};

pub const DEFAULT_URL: &str = "wss://api.deepgram.com/v1/listen";

/// Deepgram drops an idle stream after ~10 s; ping it well inside that.
const KEEPALIVE: Duration = Duration::from_secs(4);

/// Deepgram language code used when neither the page nor `STT_LANGUAGE` picks one.
pub const DEFAULT_LANGUAGE: &str = "en";

/// Every language the page offers — English, `multi`, and the Indian languages — is one nova-3
/// transcribes. A code nova-3 does not know is a 400 from Deepgram when the call connects.
const MODEL: &str = "nova-3";

/// Turn-taking knobs, chosen per call from the web page so they can be tuned by ear.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(default)]
pub struct Endpointing {
    /// Silence (ms) that ends a segment. Lower replies sooner but cuts off pauses.
    pub endpointing: u32,
    /// Gap (ms) with no new words before the turn is declared over. Backstop for `endpointing`,
    /// not a latency lever: Deepgram says to keep it at 1000+ because interim results only
    /// arrive about once a second. Lower values are passed through anyway, to be tried by ear.
    pub utterance_end_ms: u32,
}

impl Default for Endpointing {
    fn default() -> Self {
        Self { endpointing: 300, utterance_end_ms: 1000 }
    }
}

impl Endpointing {
    /// Keeps values inside what Deepgram accepts, whatever the page sends.
    pub fn clamped(self) -> Self {
        Self {
            endpointing: self.endpointing.clamp(10, 5_000),
            utterance_end_ms: self.utterance_end_ms.clamp(100, 10_000),
        }
    }
}

/// What the transcriber tells the rest of the pipeline.
#[derive(Debug, Clone)]
pub enum SttEvent {
    /// Caller started speaking (used for barge-in).
    SpeechStarted,
    /// Best guess so far for the current utterance. `lag` is `None` when it can't be measured.
    Interim { text: String, lag: Option<Duration> },
    /// Stable transcript for a finished segment.
    Final { text: String, lag: Option<Duration> },
    /// Caller stopped talking: the turn is over.
    UtteranceEnd,
}

/// A Deepgram socket that is connected and waiting for audio.
///
/// Connecting takes a TLS handshake and a round trip to Deepgram, so a call warms one up while
/// WebRTC is still negotiating. [`Session::run`] then starts streaming with no further delay.
pub struct Session {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

/// Opens the socket. Deepgram closes an idle stream after about 10 seconds, so a session that
/// is warmed but not yet fed is kept alive by [`Session::run`].
pub async fn connect(
    url: &str,
    api_key: &str,
    endpointing: Endpointing,
    language: &str,
) -> Result<Session> {
    let endpointing = endpointing.clamped();
    let params = [
        ("model", MODEL),
        ("language", language),
        ("encoding", "linear16"),
        ("sample_rate", &SAMPLE_RATE.to_string()),
        ("channels", "1"),
        ("interim_results", "true"),
        ("smart_format", "true"),
        ("punctuate", "true"),
        // Silence that ends a segment, and the longer gap that ends the whole turn.
        ("endpointing", &endpointing.endpointing.to_string()),
        ("utterance_end_ms", &endpointing.utterance_end_ms.to_string()),
        ("vad_events", "true"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={v}"))
    .collect::<Vec<_>>()
    .join("&");

    let mut request = format!("{url}?{params}").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Token {api_key}").parse()?);

    let started = Instant::now();
    let (socket, response) = connect_async(request)
        .await
        .context("Deepgram connect failed (check DEEPGRAM_API_KEY)")?;
    debug!("deepgram handshake: {}", response.status());
    info!(
        "deepgram warm in {} ms ({MODEL}, language {}, endpointing {} ms, utterance end {} ms)",
        started.elapsed().as_millis(),
        language,
        endpointing.endpointing,
        endpointing.utterance_end_ms,
    );
    Ok(Session { socket })
}

impl Session {
    /// Streams `mic` audio and forwards transcript events until either side stops.
    pub async fn run(
        self,
        mut mic: mpsc::Receiver<Frame>,
        events: mpsc::Sender<SttEvent>,
    ) -> Result<()> {
        // Records when each moment of audio was handed over, so a result's lag is measured
        // against the audio it actually describes.
        let clock = Arc::new(AudioClock::default());
        let (mut tx, mut rx) = self.socket.split();

        let uplink = {
            let clock = Arc::clone(&clock);
            async move {
                let mut keepalive = tokio::time::interval(KEEPALIVE);
                keepalive.tick().await;
                loop {
                    tokio::select! {
                        frame = mic.recv() => {
                            let Some(frame) = frame else { break };
                            let seconds = frame.len() as f64 / SAMPLE_RATE as f64;
                            let pcm: Vec<u8> = frame
                                .iter()
                                .flat_map(|s| {
                                    ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes()
                                })
                                .collect();
                            tx.send(Message::Binary(pcm.into())).await?;
                            clock.sent(seconds);
                            keepalive.reset();
                        }
                        // Keeps a warmed-but-idle stream (and any silent gap) from being closed.
                        _ = keepalive.tick() => {
                            tx.send(Message::Text(r#"{"type":"KeepAlive"}"#.into())).await?;
                        }
                    }
                }
                tx.send(Message::Text(r#"{"type":"CloseStream"}"#.into())).await?;
                Ok::<_, anyhow::Error>(())
            }
        };

        let downlink = async move {
            while let Some(message) = rx.next().await {
                let Message::Text(text) = message? else { continue };
                match serde_json::from_str::<Response>(&text) {
                    Ok(response) => {
                        let lag = response.lag(&clock);
                        debug!("deepgram lag: {lag:?}");
                        for event in response.into_events(lag) {
                            if events.send(event).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => warn!("unparsed deepgram message ({e}): {text}"),
                }
            }
            Err(anyhow!("deepgram closed the connection"))
        };

        tokio::try_join!(uplink, downlink).map(|_| ())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Response {
    Results {
        channel: Channel,
        is_final: bool,
        #[serde(default)]
        speech_final: bool,
        /// Start of this segment on the audio timeline, in seconds.
        #[serde(default)]
        start: f64,
        /// Length of this segment, in seconds.
        #[serde(default)]
        duration: f64,
    },
    SpeechStarted {},
    UtteranceEnd {},
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Channel {
    alternatives: Vec<Alternative>,
}

#[derive(Deserialize)]
struct Alternative {
    transcript: String,
}

impl Response {
    /// How far this result trails the audio it describes: now, minus the moment that audio was
    /// sent. `None` when the result carries no usable audio timestamps.
    fn lag(&self, clock: &AudioClock) -> Option<Duration> {
        let Response::Results { start, duration, .. } = self else {
            return None;
        };
        let audio_end = start + duration;
        if !audio_end.is_finite() || audio_end <= 0.0 {
            return None;
        }
        clock.sent_at(audio_end).map(|at| at.elapsed())
    }

    fn into_events(self, lag: Option<Duration>) -> Vec<SttEvent> {
        match self {
            Response::SpeechStarted {} => vec![SttEvent::SpeechStarted],
            Response::UtteranceEnd {} => vec![SttEvent::UtteranceEnd],
            Response::Results { channel, is_final, speech_final, .. } => {
                let transcript = channel
                    .alternatives
                    .into_iter()
                    .next()
                    .map(|a| a.transcript)
                    .unwrap_or_default();
                if transcript.is_empty() {
                    return vec![];
                }
                let mut events = vec![if is_final {
                    SttEvent::Final { text: transcript, lag }
                } else {
                    SttEvent::Interim { text: transcript, lag }
                }];
                // speech_final means Deepgram itself detected the end of speech.
                if speech_final {
                    events.push(SttEvent::UtteranceEnd);
                }
                events
            }
            Response::Other => vec![],
        }
    }
}

/// When each moment of audio was handed to the service.
///
/// Audio is not always sent in real time — the browser can deliver a burst of buffered packets —
/// so lag cannot be inferred from elapsed time alone. This keeps a mark per frame and looks up
/// the instant a given point on the audio timeline went out.
#[derive(Default)]
struct AudioClock(Mutex<Timeline>);

/// About two minutes of 20 ms frames.
const MAX_MARKS: usize = 6_000;

#[derive(Default)]
struct Timeline {
    /// Total audio sent so far, in seconds.
    sent: f64,
    /// `(audio seconds sent after this frame, when it was sent)`, oldest first.
    marks: VecDeque<(f64, Instant)>,
}

impl AudioClock {
    fn sent(&self, seconds: f64) {
        let mut timeline = self.0.lock().unwrap();
        timeline.sent += seconds;
        let sent = timeline.sent;
        timeline.marks.push_back((sent, Instant::now()));
        if timeline.marks.len() > MAX_MARKS {
            timeline.marks.pop_front();
        }
    }

    /// The instant the audio reaching `audio_end` seconds was sent, or `None` if the service
    /// referred to audio we have not sent yet.
    fn sent_at(&self, audio_end: f64) -> Option<Instant> {
        let timeline = self.0.lock().unwrap();
        timeline
            .marks
            .iter()
            .find(|(sent, _)| *sent >= audio_end)
            .map(|(_, at)| *at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 ms frames, as the pipeline sends them.
    const FRAME: f64 = 0.02;


    #[test]
    fn lag_is_measured_from_when_that_audio_was_sent() {
        let clock = AudioClock::default();
        clock.sent(FRAME);
        std::thread::sleep(Duration::from_millis(50));
        clock.sent(FRAME);

        // A result covering only the first frame is dated from the first frame, so its lag
        // includes the 50 ms that have passed since.
        let lag = clock.sent_at(FRAME).unwrap().elapsed();
        assert!(lag >= Duration::from_millis(50), "{lag:?}");
        // A result covering both frames is dated from the second, so its lag is much smaller.
        assert!(clock.sent_at(FRAME * 2.0).unwrap().elapsed() < lag);
    }

    #[test]
    fn a_burst_of_audio_does_not_collapse_lag_to_zero() {
        let clock = AudioClock::default();
        // Two seconds of audio handed over in one go, as a browser burst does.
        for _ in 0..100 {
            clock.sent(FRAME);
        }
        std::thread::sleep(Duration::from_millis(30));

        // Elapsed time is far less than the audio duration; lag must still be real, not zero.
        let lag = clock.sent_at(2.0).unwrap().elapsed();
        assert!(lag >= Duration::from_millis(30), "{lag:?}");
    }

    #[test]
    fn audio_we_have_not_sent_is_unmeasurable_rather_than_zero() {
        let clock = AudioClock::default();
        clock.sent(FRAME);
        assert!(clock.sent_at(5.0).is_none());
    }

    #[test]
    fn endpointing_from_the_page_is_clamped_to_what_deepgram_accepts() {
        let wild = Endpointing { endpointing: 0, utterance_end_ms: 10 }.clamped();
        assert_eq!(wild.endpointing, 10);
        assert_eq!(wild.utterance_end_ms, 100);

        let huge = Endpointing { endpointing: 99_999, utterance_end_ms: 99_999 }.clamped();
        assert_eq!(huge.endpointing, 5_000);
        assert_eq!(huge.utterance_end_ms, 10_000);

        let sane = Endpointing { endpointing: 800, utterance_end_ms: 2_500 };
        assert_eq!(sane.clamped().endpointing, 800);
        assert_eq!(sane.clamped().utterance_end_ms, 2_500);
    }

    #[test]
    fn results_without_timestamps_have_no_lag() {
        let clock = AudioClock::default();
        clock.sent(FRAME);
        let response: Response = serde_json::from_str(
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"hi"}]}}"#,
        )
        .unwrap();
        assert!(response.lag(&clock).is_none());
    }
}
