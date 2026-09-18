//! Runtime configuration, read once from the environment (`.env` is loaded at startup).

use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Overridden with `OPENAI_MODEL`; pick whichever fast chat model the account has.
const DEFAULT_MODEL: &str = "gpt-5.2-mini";

pub struct Config {
    /// Shared across calls so its connection pool is reused.
    pub http: reqwest::Client,

    // ---- services
    pub deepgram_key: String,
    /// Override for the Deepgram endpoint (on-prem deployments, or a mock during tests).
    pub deepgram_url: String,
    pub openai_key: String,
    /// OpenAI-compatible API root, e.g. `https://api.openai.com/v1`.
    pub openai_base_url: String,
    /// Model ids change often, so this is configurable rather than baked in.
    pub openai_model: String,
    pub murf_key: String,
    pub murf_url: String,
    pub murf_voice: String,
    /// Default instructions for the model. Calls may override it from the page.
    pub system_prompt: String,
    /// Default Deepgram language code (`en`, `multi`, `es`, …). Calls may override it.
    pub stt_language: String,

    // ---- server
    /// Where the HTTP server listens.
    pub addr: String,
    /// Required on `POST /offer` when set. Anyone who can start a call spends API credit.
    pub access_token: Option<String>,
    /// Lets the page point the model at another endpoint. Off by default: the server sends
    /// `OPENAI_API_KEY` to whatever URL it is given.
    pub allow_base_url_override: bool,
    /// Calls beyond this are refused with 503, capping concurrent spend.
    pub max_calls: usize,
    /// Calls are ended after this long, so a forgotten tab cannot run up a bill.
    pub max_call_duration: Duration,

    // ---- WebRTC networking
    /// Local address the media sockets bind to. Defaults to the address of the default route.
    pub rtc_bind_ip: Option<IpAddr>,
    /// Address advertised to browsers. Set it on cloud hosts, where the machine only knows its
    /// private address and browsers must be told the public one.
    pub public_ip: Option<IpAddr>,
    /// UDP ports for media, one per call, so a firewall can open a known range. `None` lets the
    /// OS choose, which only works without a firewall in the way.
    pub rtc_ports: Option<RangeInclusive<u16>>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let rtc_ports = match (optional::<u16>("RTC_PORT_MIN")?, optional::<u16>("RTC_PORT_MAX")?) {
            (Some(min), Some(max)) if min <= max => Some(min..=max),
            (None, None) => None,
            _ => bail!("RTC_PORT_MIN and RTC_PORT_MAX must both be set, with MIN <= MAX"),
        };
        let max_calls = optional("MAX_CALLS")?.unwrap_or(10);
        if let Some(ports) = &rtc_ports
            && ports.len() < max_calls
        {
            bail!(
                "RTC_PORT_MIN..RTC_PORT_MAX has {} ports but MAX_CALLS is {max_calls}: each call needs its own",
                ports.len()
            );
        }

        Ok(Self {
            http: crate::llm::client(),
            deepgram_key: var("DEEPGRAM_API_KEY")?,
            deepgram_url: or("DEEPGRAM_URL", crate::stt::DEFAULT_URL),
            openai_key: var("OPENAI_API_KEY")?,
            openai_base_url: or("OPENAI_BASE_URL", crate::llm::DEFAULT_BASE_URL),
            openai_model: or("OPENAI_MODEL", DEFAULT_MODEL),
            murf_key: var("MURF_API_KEY")?,
            murf_url: or("MURF_URL", crate::tts::DEFAULT_URL),
            murf_voice: or("MURF_VOICE", crate::tts::DEFAULT_VOICE),
            system_prompt: or("SYSTEM_PROMPT", crate::llm::SYSTEM_PROMPT),
            stt_language: or("STT_LANGUAGE", crate::stt::DEFAULT_LANGUAGE),

            addr: or("ADDR", "127.0.0.1:8080"),
            access_token: std::env::var("ACCESS_TOKEN").ok().filter(|t| !t.is_empty()),
            allow_base_url_override: optional("ALLOW_BASE_URL_OVERRIDE")?.unwrap_or(false),
            max_calls,
            max_call_duration: Duration::from_secs(optional("MAX_CALL_SECS")?.unwrap_or(1800)),

            rtc_bind_ip: optional("RTC_BIND_IP")?,
            public_ip: optional("PUBLIC_IP")?,
            rtc_ports,
        })
    }
}

fn var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set (put it in .env)"))
}

fn or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// An optional typed variable: unset is `None`, but a value that does not parse is an error
/// rather than a silent fallback to the default.
fn optional<T: FromStr>(name: &str) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{name}={value:?} is invalid: {e}")),
        _ => Ok(None),
    }
}
