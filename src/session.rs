//! One WebRTC call: browser audio in, agent audio out.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::peer_connection::transport::RTCIceCandidateType;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::PayloadType;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::rtp_transceiver::RtpSender;

use crate::agent::{self, AgentIo};
use crate::audio::{self, Frame, Speaker};
use crate::metrics::TurnMetrics;
use crate::config::Config;
use crate::stt;
use crate::task::AbortOnDrop;
use crate::tts;

/// Per-call settings chosen on the web page and sent as query parameters with the offer.
#[derive(serde::Deserialize)]
#[serde(default)]
pub struct CallSettings {
    pub endpointing: u32,
    pub utterance_end_ms: u32,
    /// Empty means the server default from `OPENAI_MODEL`.
    pub model: String,
    /// OpenAI-compatible API root. Empty means the server default from `OPENAI_BASE_URL`.
    pub base_url: String,
    /// Murf voice id. Empty means the server default from `MURF_VOICE`.
    pub voice: String,
    /// Instructions for the model. Empty means the server default from `SYSTEM_PROMPT`.
    pub system_prompt: String,
    /// Sampling temperature, as typed. Empty means "don't send one": several models only accept
    /// their own default and reject the parameter outright.
    pub temperature: String,
    /// Deepgram language code. Empty means the server default from `STT_LANGUAGE`.
    pub language: String,
}

/// A system prompt is resent with every turn, so an enormous one costs latency on each reply.
const MAX_SYSTEM_PROMPT: usize = 2000;

impl CallSettings {
    /// Rejects settings the caller got wrong, so they come back as 400 rather than 500.
    pub fn validate(&self, allow_base_url_override: bool) -> Result<(), String> {
        if self.system_prompt.chars().count() > MAX_SYSTEM_PROMPT {
            return Err(format!("the system prompt must be under {MAX_SYSTEM_PROMPT} characters"));
        }
        self.parsed_temperature()?;
        // Deepgram's codes are letters and an optional region, e.g. `en`, `multi`, `pt-BR`.
        if !self.language.is_empty()
            && !self.language.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
        {
            return Err("language must be a Deepgram language code, such as en or multi".to_owned());
        }

        let base_url = self.base_url.trim();
        if base_url.is_empty() {
            return Ok(());
        }
        // Off by default: the server would send its OpenAI key to whatever URL is named.
        if !allow_base_url_override {
            return Err("this server does not allow overriding the base URL".to_owned());
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err("base URL must start with http:// or https://".to_owned());
        }
        Ok(())
    }

    /// `None` leaves the parameter out, so the model uses its own default.
    fn parsed_temperature(&self) -> Result<Option<f32>, String> {
        match self.temperature.trim() {
            "" => Ok(None),
            value => match value.parse::<f32>() {
                Ok(t) if (0.0..=2.0).contains(&t) => Ok(Some(t)),
                _ => Err("temperature must be a number between 0 and 2".to_owned()),
            },
        }
    }
}

impl Default for CallSettings {
    fn default() -> Self {
        let stt::Endpointing { endpointing, utterance_end_ms } = stt::Endpointing::default();
        Self {
            endpointing,
            utterance_end_ms,
            model: String::new(),
            base_url: String::new(),
            voice: String::new(),
            system_prompt: String::new(),
            temperature: String::new(),
            language: String::new(),
        }
    }
}

const OPUS_PAYLOAD_TYPE: PayloadType = 111;
const FRAME_DURATION: Duration = Duration::from_millis(20);
/// How long a `Disconnected` peer is given to come back before the call is torn down.
/// WebRTC recovers from brief network blips, but a closed tab never returns.
const DISCONNECT_GRACE: Duration = Duration::from_secs(5);

struct Handler {
    gathered: mpsc::Sender<()>,
    connected: mpsc::Sender<()>,
    disconnected: mpsc::Sender<()>,
    closed: mpsc::Sender<()>,
    mic: mpsc::Sender<Frame>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        info!("peer connection state: {state}");
        match state {
            RTCPeerConnectionState::Connected => {
                let _ = self.connected.try_send(());
            }
            // Disconnected may recover, so it starts a grace period rather than ending the call.
            RTCPeerConnectionState::Disconnected => {
                let _ = self.disconnected.try_send(());
            }
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                let _ = self.closed.try_send(());
            }
            _ => {}
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if track.kind().await != RtpCodecKind::Audio {
            return;
        }
        info!("caller audio track opened");
        let mic = self.mic.clone();
        tokio::spawn(async move {
            let mut decoder = audio::Decoder::new();
            while let Some(event) = track.poll().await {
                let TrackRemoteEvent::OnRtpPacket(packet) = event else {
                    continue;
                };
                // Padding-only packets (e.g. while the browser stops the track).
                if packet.payload.is_empty() {
                    continue;
                }
                match decoder.decode(&packet.payload) {
                    Ok(frame) => {
                        if mic.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("{e:#}"),
                }
            }
            info!("caller audio track ended");
        });
    }
}

/// Builds a peer connection for the browser's offer and returns the complete answer
/// (non-trickle ICE). The call keeps running in the background until it disconnects.
pub async fn accept_offer(
    config: Arc<Config>,
    settings: CallSettings,
    offer: RTCSessionDescription,
    permit: OwnedSemaphorePermit,
) -> Result<RTCSessionDescription> {
    let endpointing = stt::Endpointing {
        endpointing: settings.endpointing,
        utterance_end_ms: settings.utterance_end_ms,
    };
    let model = match settings.model.trim() {
        "" => config.openai_model.clone(),
        chosen => chosen.to_owned(),
    };
    let base_url = match settings.base_url.trim() {
        "" => config.openai_base_url.clone(),
        chosen => {
            // Validated against ALLOW_BASE_URL_OVERRIDE already; still worth saying out loud,
            // since the API key goes wherever this points.
            warn!("call overrides the LLM endpoint: {chosen}");
            chosen.to_owned()
        }
    };
    // Validated above, so the only way here is a value the model accepts.
    let temperature = settings.parsed_temperature().unwrap_or_default();
    let system_prompt = match settings.system_prompt.trim() {
        "" => config.system_prompt.clone(),
        chosen => chosen.to_owned(),
    };
    let language = match settings.language.trim() {
        "" => config.stt_language.clone(),
        chosen => chosen.to_owned(),
    };
    info!(
        "llm model: {model} at {base_url} (temperature {})",
        temperature.map_or_else(|| "model default".to_owned(), |t| t.to_string())
    );

    let llm = crate::llm::Llm::new(
        config.http.clone(),
        &base_url,
        config.openai_key.clone(),
        model,
    )
    .with_temperature(temperature);

    // Warm the model's connection too: DNS, TLS and HTTP/2 setup off the critical path.
    tokio::spawn({
        let llm = llm.clone();
        async move { llm.warm().await }
    });

    // Warm the voice up too, alongside ICE/DTLS.
    let voice = match settings.voice.trim() {
        "" => config.murf_voice.clone(),
        chosen => chosen.to_owned(),
    };
    let tts = AbortOnDrop::new(tokio::spawn({
        let config = Arc::clone(&config);
        async move { tts::connect(&config.murf_url, &config.murf_key, &voice).await }
    }));
    let opus = RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: audio::SAMPLE_RATE as u32,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    };

    // Warm the transcriber up now: its TLS handshake and round trip to Deepgram run alongside
    // ICE and DTLS, so the first word of the call is not waiting on a connection.
    let stt = AbortOnDrop::new(tokio::spawn({
        let config = Arc::clone(&config);
        async move {
            stt::connect(&config.deepgram_url, &config.deepgram_key, endpointing, &language).await
        }
    }));

    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: opus.clone(),
            payload_type: OPUS_PAYLOAD_TYPE,
        },
        RtpCodecKind::Audio,
    )?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

    let (gathered_tx, mut gathered_rx) = mpsc::channel(1);
    let (connected_tx, connected_rx) = mpsc::channel(1);
    let (disconnected_tx, disconnected_rx) = mpsc::channel(1);
    let (closed_tx, closed_rx) = mpsc::channel(1);
    let (mic_tx, mic_rx) = mpsc::channel(50);

    let handler = Arc::new(Handler {
        gathered: gathered_tx,
        connected: connected_tx,
        disconnected: disconnected_tx,
        closed: closed_tx,
        mic: mic_tx,
    });

    let mut setting_engine = SettingEngine::default();
    if let Some(public_ip) = config.public_ip {
        // On a cloud host the machine only knows its private address; tell browsers the public one.
        setting_engine.set_nat_1to1_ips(vec![public_ip.to_string()], RTCIceCandidateType::Host);
    }

    let pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media_engine)
            .with_setting_engine(setting_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_udp_addrs(vec![media_addr(&config)?])
            .build()
            .await?,
    );

    let track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
        "sol-li-ni".to_owned(),
        "sol-li-ni-audio".to_owned(),
        "agent".to_owned(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(rand::random::<u32>()),
                ..Default::default()
            },
            codec: opus,
            ..Default::default()
        }],
    ))?);
    let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;

    tokio::time::timeout(Duration::from_secs(5), gathered_rx.recv())
        .await
        .context("ICE gathering timed out")?;
    let answer = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no local description"))?;

    tokio::spawn(run_call(Call {
        config,
        llm,
        system_prompt,
        stt: stt.keep(),
        tts: tts.keep(),
        pc,
        track,
        sender,
        connected: connected_rx,
        disconnected: disconnected_rx,
        closed: closed_rx,
        mic: mic_rx,
        _permit: permit,
    }));
    Ok(answer)
}

/// Everything a negotiated call needs to run.
struct Call {
    config: Arc<Config>,
    llm: crate::llm::Llm,
    system_prompt: String,
    stt: JoinHandle<Result<stt::Session>>,
    tts: JoinHandle<Result<tts::Session>>,
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    connected: mpsc::Receiver<()>,
    disconnected: mpsc::Receiver<()>,
    closed: mpsc::Receiver<()>,
    mic: mpsc::Receiver<Frame>,
    /// Held for the life of the call; dropping it frees a slot for the next caller.
    _permit: OwnedSemaphorePermit,
}

async fn run_call(call: Call) {
    let Call {
        config,
        llm,
        system_prompt,
        stt,
        tts,
        pc,
        track,
        sender,
        mut connected,
        mut disconnected,
        mut closed,
        mic,
        _permit,
    } = call;
    let max_duration = config.max_call_duration;
    let result = async {
        tokio::select! {
            _ = connected.recv() => {}
            _ = closed.recv() => {
                stt.abort();
                tts.abort();
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                stt.abort();
                tts.abort();
                return Err(anyhow!("connection timed out"));
            }
        }

        // Shared so the agent can flush it when the caller interrupts.
        let metrics = Arc::new(std::sync::Mutex::new(TurnMetrics::default()));
        let speaker = Arc::new(Speaker::new(Arc::clone(&metrics)));
        // Guarded: an error below returns early, and the agent (with its Deepgram and Murf
        // sockets) must not outlive the call either way.
        let _agent = AbortOnDrop::new(tokio::spawn(agent::run(
            llm,
            system_prompt,
            stt,
            tts,
            AgentIo { mic, speaker: Arc::clone(&speaker) },
            metrics,
        )));

        let audio = send_audio(&track, &sender, &speaker);
        tokio::pin!(audio);
        let deadline = tokio::time::sleep(max_duration);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                result = &mut audio => break result?,
                _ = closed.recv() => break,
                _ = &mut deadline => {
                    info!("call reached its {} s limit", max_duration.as_secs());
                    break;
                }
                _ = disconnected.recv() => {
                    // Wait it out: a blip reconnects, a closed tab does not.
                    tokio::select! {
                        _ = connected.recv() => info!("peer reconnected"),
                        _ = closed.recv() => break,
                        result = &mut audio => break result?,
                        _ = tokio::time::sleep(DISCONNECT_GRACE) => {
                            info!("peer gone for {}s", DISCONNECT_GRACE.as_secs());
                            break;
                        }
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    if let Err(e) = result {
        warn!("call ended with error: {e:#}");
    }
    let _ = pc.close().await;
    info!("call closed");
}

/// Sends one Opus packet every 20 ms: queued agent audio, or silence when idle so the
/// browser's jitter buffer keeps a steady clock.
async fn send_audio(
    track: &TrackLocalStaticSample,
    sender: &Arc<dyn RtpSender>,
    speaker: &Speaker,
) -> Result<()> {
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or_else(|| anyhow!("sender has no negotiated codec"))?;
    let ssrc = *track
        .ssrcs()
        .await
        .first()
        .ok_or_else(|| anyhow!("track has no ssrc"))?;

    let mut encoder = audio::Encoder::new()?;
    let silence = vec![0.0; audio::FRAME_SAMPLES];
    let mut ticker = tokio::time::interval(FRAME_DURATION);

    loop {
        ticker.tick().await;
        let frame = speaker.pop().unwrap_or_else(|| silence.clone());
        let sample = Sample {
            data: encoder.encode(&frame)?,
            duration: FRAME_DURATION,
            ..Default::default()
        };
        track.write_sample(ssrc, payload_type, &sample, &[]).await?;
    }
}

/// LAN address of the default route, used as the host ICE candidate.
/// The address this call's media socket binds to: `RTC_BIND_IP` (or the default-route address)
/// and, with `RTC_PORT_MIN`/`RTC_PORT_MAX` set, a free port from that range so a firewall can
/// open a known range.
fn media_addr(config: &Config) -> Result<String> {
    let ip = match config.rtc_bind_ip {
        Some(ip) => ip,
        None => local_ip()?,
    };
    let Some(ports) = &config.rtc_ports else {
        return Ok(format!("{}", SocketAddr::new(ip, 0)));
    };

    // Round-robin from where the last call left off, so recently freed ports rest a moment.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let (first, count) = (*ports.start() as usize, ports.len());
    let start = NEXT.fetch_add(1, Ordering::Relaxed);
    for offset in 0..count {
        let port = (first + (start + offset) % count) as u16;
        // Probe: the peer connection binds it for real a moment later. The call limit keeps
        // this range at least as large as the number of calls, so races are rare and harmless.
        if UdpSocket::bind(SocketAddr::new(ip, port)).is_ok() {
            return Ok(SocketAddr::new(ip, port).to_string());
        }
    }
    bail!("no free UDP port in {}..={}", ports.start(), ports.end())
}

fn local_ip() -> Result<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:80")?;
    Ok(socket.local_addr()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> CallSettings {
        CallSettings::default()
    }

    #[test]
    fn a_blank_temperature_means_the_model_decides() {
        assert_eq!(settings().parsed_temperature(), Ok(None));

        let set = CallSettings { temperature: "0.85".to_owned(), ..settings() };
        assert_eq!(set.parsed_temperature(), Ok(Some(0.85)));
    }

    #[test]
    fn a_temperature_outside_what_the_api_accepts_is_a_400_not_a_500() {
        for bad in ["2.5", "-1", "warm"] {
            let set = CallSettings { temperature: bad.to_owned(), ..settings() };
            assert!(set.validate(false).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn a_system_prompt_is_capped_so_every_turn_does_not_carry_a_novel() {
        let long = CallSettings {
            system_prompt: "a".repeat(MAX_SYSTEM_PROMPT + 1),
            ..settings()
        };
        assert!(long.validate(false).is_err());

        let fine = CallSettings { system_prompt: "be terse".to_owned(), ..settings() };
        assert!(fine.validate(false).is_ok());
    }

    #[test]
    fn a_language_must_look_like_a_deepgram_code() {
        let ok = CallSettings { language: "pt-BR".to_owned(), ..settings() };
        assert!(ok.validate(false).is_ok());

        let injected = CallSettings { language: "en&model=nova-2".to_owned(), ..settings() };
        assert!(injected.validate(false).is_err());
    }
}
