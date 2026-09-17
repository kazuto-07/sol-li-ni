//! Latency accounting for the services in the pipeline.
//!
//! Only service latency is tracked — how long Deepgram (and later the LLM and TTS) take —
//! never how long the caller spoke.

use std::time::{Duration, Instant};

use tracing::info;

#[derive(Default)]
pub struct TurnMetrics {
    /// Transcription lag of each interim result this turn, where it could be measured.
    partials: Vec<Duration>,
    /// Interim results whose lag could not be measured.
    partials_unknown: u32,
    /// When the last final transcript arrived, and how far it trailed the audio.
    last_final: Option<(Instant, Option<Duration>)>,
    /// When the caller's turn ended: the clock the reply is measured against.
    turn_ended: Option<Instant>,
    /// First token from the language model, relative to the turn ending.
    llm_first_token: Option<Duration>,
}

impl TurnMetrics {
    pub fn interim(&mut self, lag: Option<Duration>) {
        match lag {
            Some(lag) => self.partials.push(lag),
            None => self.partials_unknown += 1,
        }
    }

    pub fn transcript_final(&mut self, lag: Option<Duration>) {
        self.last_final = Some((Instant::now(), lag));
    }

    /// First token of the reply.
    pub fn llm_first_token(&mut self) {
        self.llm_first_token = self.turn_ended.map(|end| end.elapsed());
    }

    /// The reply finished streaming. Logs what the model cost end to end.
    pub fn llm_done(&mut self) {
        let Some(end) = self.turn_ended else { return };
        let mut parts = Vec::new();
        if let Some(first_token) = self.llm_first_token {
            parts.push(format!("first token {}", ms(first_token)));
        }
        parts.push(format!("full reply {}", ms(end.elapsed())));
        info!("llm latency: {}", parts.join(" | "));
    }

    /// First reply audio handed to the caller. Logs the end-to-end latency: from the caller
    /// falling silent to hearing the agent. Later frames of the same turn do nothing.
    pub fn response_audible(&mut self) {
        let Some(end) = self.turn_ended.take() else { return };
        let mut parts = vec![format!("overall {}", ms(end.elapsed()))];
        if let Some(first_token) = self.llm_first_token {
            parts.push(format!("llm first token {}", ms(first_token)));
        }
        info!("response metrics: {}", parts.join(" | "));
    }

    /// The caller talked over the agent. `dropped` is how much queued speech was thrown away.
    pub fn interrupted(&mut self, dropped: Duration) {
        info!("barge-in: dropped {} of queued speech", ms(dropped));
        self.turn_ended = None;
        self.llm_first_token = None;
    }

    /// Logs Deepgram's latency for the turn that just ended, and starts the reply clock.
    pub fn utterance_end(&mut self) {
        let now = Instant::now();
        let mut parts = Vec::new();

        if !self.partials.is_empty() {
            let count = self.partials.len() as u32;
            let avg = self.partials.iter().sum::<Duration>() / count;
            let max = self.partials.iter().max().copied().unwrap_or_default();
            parts.push(format!("partial avg {} (max {}, n={count})", ms(avg), ms(max)));
        }
        if self.partials_unknown > 0 {
            parts.push(format!("partial unmeasured n={}", self.partials_unknown));
        }
        match self.last_final {
            Some((at, Some(lag))) => {
                parts.push(format!("final {}", ms(lag)));
                // How long after the caller stopped talking Deepgram said the turn had ended.
                parts.push(format!("endpoint {}", ms(now.saturating_duration_since(at - lag))));
            }
            Some((_, None)) => parts.push("final unmeasured".to_owned()),
            None => {}
        }

        if !parts.is_empty() {
            info!("deepgram latency: {}", parts.join(" | "));
        }
        *self = Self { turn_ended: Some(now), ..Default::default() };
    }
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}
