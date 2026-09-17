//! Opus <-> PCM. WebRTC carries Opus at 48 kHz; the agent works on 20 ms mono f32 frames.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use opus_rs::{Application, OpusDecoder, OpusEncoder};

use std::time::Duration;

pub const SAMPLE_RATE: usize = 48_000;
/// 20 ms at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;

/// One 20 ms mono frame at 48 kHz.
pub type Frame = Vec<f32>;

/// Audio waiting to be sent to the caller.
///
/// A queue rather than a channel because barge-in has to throw away everything not yet played:
/// with a channel, frames already queued would keep playing after the agent was interrupted.
pub struct Speaker {
    frames: Mutex<VecDeque<Frame>>,
    metrics: Arc<Mutex<crate::metrics::TurnMetrics>>,
}

/// About 30 seconds of audio. A reply that outruns this is a bug somewhere upstream.
const MAX_QUEUED: usize = 1_500;

impl Speaker {
    pub fn new(metrics: Arc<Mutex<crate::metrics::TurnMetrics>>) -> Self {
        Self { frames: Mutex::new(VecDeque::new()), metrics }
    }

    pub fn push(&self, frame: Frame) {
        // The first frame of a turn is the moment the caller can hear the agent.
        self.metrics.lock().expect("metrics lock").response_audible();

        let mut frames = self.frames.lock().expect("speaker lock");
        if frames.len() >= MAX_QUEUED {
            frames.pop_front();
        }
        frames.push_back(frame);
    }

    pub fn pop(&self) -> Option<Frame> {
        self.frames.lock().expect("speaker lock").pop_front()
    }

    /// Drops everything not yet played. Returns how much audio was discarded.
    pub fn clear(&self) -> Duration {
        let mut frames = self.frames.lock().expect("speaker lock");
        let dropped = frames.len();
        frames.clear();
        Duration::from_millis(dropped as u64 * 20)
    }
}

/// Decodes browser Opus packets to mono frames.
///
/// SDP always negotiates Opus as 2 channels, but browsers send mono-coded packets for a
/// mic. opus-rs requires the decoder's channel count to match each packet, so one decoder
/// per channel count is created on demand from the packet's TOC stereo bit.
pub struct Decoder {
    mono: Option<OpusDecoder>,
    stereo: Option<OpusDecoder>,
    buf: Vec<f32>,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            mono: None,
            stereo: None,
            // Largest Opus frame is 120 ms, times 2 channels.
            buf: vec![0.0; SAMPLE_RATE / 1000 * 120 * 2],
        }
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Frame> {
        let toc = *packet.first().ok_or_else(|| anyhow!("empty opus packet"))?;
        let channels = if toc & 0x04 != 0 { 2 } else { 1 };
        let slot = if channels == 2 { &mut self.stereo } else { &mut self.mono };
        let decoder = match slot {
            Some(d) => d,
            None => slot.insert(
                OpusDecoder::new(SAMPLE_RATE as i32, channels)
                    .map_err(|e| anyhow!("opus decoder: {e}"))?,
            ),
        };

        let max_samples = self.buf.len() / 2;
        let n = decoder
            .decode(packet, max_samples, &mut self.buf)
            .map_err(|e| anyhow!("opus decode: {e}"))?;
        Ok(if channels == 2 {
            self.buf[..n * 2].as_chunks::<2>().0.iter().map(|[l, r]| (l + r) * 0.5).collect()
        } else {
            self.buf[..n].to_vec()
        })
    }
}

/// Encodes mono 20 ms frames to Opus packets.
pub struct Encoder {
    inner: OpusEncoder,
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Result<Self> {
        let mut inner = OpusEncoder::new(SAMPLE_RATE as i32, 1, Application::Voip)
            .map_err(|e| anyhow!("opus encoder: {e}"))?;
        inner.bitrate_bps = 32_000;
        Ok(Self { inner, buf: vec![0; 1276] })
    }

    pub fn encode(&mut self, frame: &[f32]) -> Result<Bytes> {
        let n = self
            .inner
            .encode(frame, FRAME_SAMPLES, &mut self.buf)
            .map_err(|e| anyhow!("opus encode: {e}"))?;
        Ok(Bytes::copy_from_slice(&self.buf[..n]))
    }
}

#[cfg(test)]
mod speaker_tests {
    use super::*;

    fn speaker() -> Speaker {
        Speaker::new(Arc::new(Mutex::new(crate::metrics::TurnMetrics::default())))
    }

    #[test]
    fn frames_are_played_in_order() {
        let speaker = speaker();
        speaker.push(vec![1.0; FRAME_SAMPLES]);
        speaker.push(vec![2.0; FRAME_SAMPLES]);
        assert_eq!(speaker.pop().unwrap()[0], 1.0);
        assert_eq!(speaker.pop().unwrap()[0], 2.0);
        assert!(speaker.pop().is_none());
    }

    #[test]
    fn clearing_drops_everything_not_yet_played() {
        let speaker = speaker();
        for _ in 0..50 {
            speaker.push(vec![0.0; FRAME_SAMPLES]);
        }
        // 50 frames of 20 ms is the second of speech the caller will now not hear.
        assert_eq!(speaker.clear(), Duration::from_secs(1));
        assert!(speaker.pop().is_none(), "interrupted audio must not play");
    }

    #[test]
    fn a_runaway_queue_does_not_grow_without_bound() {
        let speaker = speaker();
        for _ in 0..MAX_QUEUED + 100 {
            speaker.push(vec![0.0; FRAME_SAMPLES]);
        }
        assert_eq!(speaker.clear(), Duration::from_millis(MAX_QUEUED as u64 * 20));
    }
}
