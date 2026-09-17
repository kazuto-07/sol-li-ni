# sol-li-ni

**A real-time voice agent written in Rust.**

Open a web page, tap the orb and talk. Your voice goes over WebRTC to a Rust server, which
turns it into text, writes a reply with a language model and speaks it back. Every step
streams, so the agent starts talking before it has finished thinking — and stops the moment
you talk over it.

```
 mic ──WebRTC──► speech-to-text ──► language model ──► text-to-speech ──WebRTC──► speaker
```

> **Providers are experimental.** The current build uses Deepgram (speech-to-text), OpenAI or
> any OpenAI-compatible API (language model) and Murf (text-to-speech). These were picked to
> get the pipeline working and are not final. Support for more providers and models is planned.

## How to run

**You need**
- Rust ([rustup.rs](https://rustup.rs))
- API keys for [Deepgram](https://console.deepgram.com), [OpenAI](https://platform.openai.com) and [Murf](https://murf.ai/api)
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
cp .env.example .env     # add DEEPGRAM_API_KEY, OPENAI_API_KEY, MURF_API_KEY
cargo run --release
```

Use `--release` to talk to it: audio is encoded and decoded live, and a debug build is much
slower. Plain `cargo run` is for development — it rebuilds faster and reloads the web page from
disk.

Open <http://127.0.0.1:8080>, tap the orb, allow the microphone and start talking.

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
- Choosing the provider and model per call from the page

## Contributing

Issues and pull requests are welcome — new providers especially. Before opening a PR:

```sh
cargo test
cargo clippy --all-targets
```

Each stage lives in its own module (`src/stt.rs`, `src/llm.rs`, `src/tts.rs`), so a new provider
does not need to touch the rest of the pipeline. Never commit your `.env` file.

## Learn more

- [PLAN.md](PLAN.md) — design decisions
- [WORKLOG.md](WORKLOG.md) — what was built and how it was tested

## License

[MIT](LICENSE)
