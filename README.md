# sol-li-ni

**An open-source, real-time AI voice agent written in Rust** — WebRTC, streaming speech-to-text,
LLM and text-to-speech, with barge-in. A fast, low-latency alternative to Python and Node.js
voice agent frameworks.

Open a web page, tap the orb and talk. Your voice goes over WebRTC to a Rust server, which
turns it into text, writes a reply with a language model and speaks it back. Every step
streams, so the agent starts talking before it has finished thinking — and stops the moment
you talk over it.

```
 mic ──WebRTC──► speech-to-text ──► language model ──► text-to-speech ──WebRTC──► speaker
```

**Built for Indian callers.** It transcribes English (India), Hindi, Bengali, Gujarati, Kannada,
Marathi, Punjabi, Tamil, Telugu and Urdu.

It runs on a free-tier stack: Deepgram nova-3 for the transcription, Groq for the model, Murf
for the voice. Any OpenAI-compatible API works in Groq's place — OpenAI, or a local
[Ollama](https://ollama.com) — by changing one line in `.env`.

The gear icon on the page opens the rest: the language, the system prompt that sets how the
agent behaves, the model and its temperature, the voice, and how long a pause ends your turn.
Each applies to the next call, so you can tune it by ear. See [Settings](#settings).

> **Providers are experimental.** The current build uses Deepgram (speech-to-text), Groq or any
> other OpenAI-compatible API (language model) and Murf (text-to-speech). They were picked to be
> fast and cheap to start with — Deepgram and Groq both have a free tier — and they are not
> final. Support for more providers and models is planned.

## How to run

**You need**
- Rust ([rustup.rs](https://rustup.rs))
- API keys for [Deepgram](https://console.deepgram.com), [Groq](https://console.groq.com/keys) and [Murf](https://murf.ai/api)
- Chrome, Edge or Firefox
- A C compiler (one dependency compiles C). Linux and macOS usually have one already. On Windows,
  it depends on your Rust toolchain:
  - **MSVC** (rustup's default): Visual Studio Build Tools with "Desktop development with C++",
    which the rustup installer offers to set up
  - **GNU**: MinGW — `winget install BrechtSanders.WinLibs.POSIX.UCRT`. If its tools are not on
    your PATH, copy `.cargo/config.example.toml` to `.cargo/config.toml` and set the paths

**Steps**

```sh
git clone <repo-url> sol-li-ni && cd sol-li-ni
cp .env.example .env     # add DEEPGRAM_API_KEY, OPENAI_API_KEY (your Groq key), MURF_API_KEY
cargo run --release
```

Open <http://127.0.0.1:8080>, tap the orb, allow the microphone and start talking.

Use `--release` when you actually want to talk to it: audio is encoded and decoded live, and a
debug build is much slower at it. Plain `cargo run` is the one to develop against — it links
faster, and it serves `web/index.html` from disk, so page edits need only a refresh.

**No API keys?** Run the mock services in `examples/` instead:

```sh
cargo run --example mock_deepgram
cargo run --example mock_openai
cargo run --example mock_murf
```

then point the server at them with the `DEEPGRAM_URL`, `OPENAI_BASE_URL` and `MURF_URL`
settings in `.env.example`.

**Deploying:** `cargo build --release` gives one self-contained binary (the web page is built in),
or use the `Dockerfile`. Put it behind HTTPS, set `ACCESS_TOKEN` and `PUBLIC_IP`, and open the
`RTC_PORT_MIN`–`RTC_PORT_MAX` UDP range. All settings are documented in `.env.example`.

## Settings

The gear icon in the top-right opens the call settings. They are saved in your browser, apply
when a call starts, and are locked while one is live. Anything left blank falls back to what the
server is configured with.

| Group | Setting | What it does |
|---|---|---|
| Access | Access token | Shown only when the server requires one |
| Speech to text | Language | What the caller speaks. English (India), Hindi, Bengali, Gujarati, Kannada, Marathi, Punjabi, Tamil, Telugu or Urdu |
| | Endpointing | Silence that ends your turn. **The main latency lever:** lower replies sooner, too low cuts you off mid-pause |
| | Utterance end | Backstop gap for noisy rooms. Deepgram recommends 1000 ms or more |
| Language model | Model | Model for this call, from whichever API `OPENAI_BASE_URL` points at (Groq by default) |
| | System prompt | How the agent should behave. Kept short: it is resent every turn |
| | Temperature | Off by default, which lets the model use its own — some models reject any other value |
| | Base URL | Only shown when `ALLOW_BASE_URL_OVERRIDE=true` |
| Text to speech | Voice | Murf voice id. Pick one in the caller's language, or the reply comes back in another one's accent |

Server-wide defaults for these live in `.env` (`STT_LANGUAGE`, `SYSTEM_PROMPT`, `OPENAI_MODEL`,
`MURF_VOICE`), and every setting is documented in `.env.example`.

## Why Rust instead of Python or Node?

Most voice agents are built in Python or Node. They work, but a voice call is a hard real-time
job, and that is where Rust pays off.

- **Steady audio timing.** Audio is sent in 20 ms frames. There is no garbage collector to pause
  the process mid-frame, so there are no stutters or growing delays under load.
- **Real parallelism.** Decoding audio, talking to three services and encoding the reply all run
  at once across every CPU core. Python's GIL and Node's single thread make CPU work like audio
  encoding compete with the network work that keeps latency low.
- **More calls per server.** Each call is a set of lightweight async tasks, not a process or a
  heavy runtime, so one small machine handles many calls with low memory use.
- **One binary, nothing to install.** No Python virtualenv, no `node_modules`, no separate media
  server. Copy one file and run it.
- **Bugs caught before they ship.** The compiler rules out whole classes of crashes and data
  races, which matters for a long-running server handling live calls.
- **Clean interruptions.** When you talk over the agent, the reply, queued audio and every task
  behind it are cancelled together — nothing keeps running (or billing) in the background.

## Roadmap

- More speech-to-text, language model and text-to-speech providers
- Choosing the provider per call from the page
- Indian-language voices in the voice list, to match the languages on the transcribing side
- Live captions on the page: transcripts and replies are in the server log only

## Contributing

Issues and pull requests are welcome — new providers especially. Before opening a PR:

```sh
cargo test
cargo clippy --all-targets
```

Each stage lives in its own module (`src/stt.rs`, `src/llm.rs`, `src/tts.rs`), so a new provider
does not need to touch the rest of the pipeline. Never commit your `.env` file.

## License

[MIT](LICENSE)
