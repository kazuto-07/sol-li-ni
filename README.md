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

## How to run it

No Rust knowledge needed — you will not touch the code. Everything below is typed into a
terminal (Command Prompt or PowerShell on Windows, Terminal on macOS and Linux).

### 1. Install Rust

Go to [rustup.rs](https://rustup.rs) and follow the one-line instruction for your system. On
Windows it also offers to install Visual Studio Build Tools — say yes, the project needs them.
Close the terminal and open a new one, then check it worked:

```sh
cargo --version
```

### 2. Get three API keys

All three are free to start with. Sign up and copy the key from each:

| Service | What it does | Where |
|---|---|---|
| Deepgram | Turns your speech into text | [console.deepgram.com](https://console.deepgram.com) → API Keys |
| Groq | Writes the reply | [console.groq.com/keys](https://console.groq.com/keys) |
| Murf | Speaks the reply aloud | [murf.ai/api](https://murf.ai/api) → API |

### 3. Download the project

```sh
git clone <repo-url> sol-li-ni
cd sol-li-ni
```

### 4. Put your keys in a file

The project reads its keys from a file named `.env`. Make one by copying the example:

```sh
cp .env.example .env         # on Windows: copy .env.example .env
```

Open `.env` in any text editor and paste each key after the `=`, with no quotes and no spaces:

```sh
DEEPGRAM_API_KEY=your-deepgram-key-here
OPENAI_API_KEY=your-groq-key-here
MURF_API_KEY=your-murf-key-here
```

`OPENAI_API_KEY` holding a Groq key is not a mistake: Groq uses the same API format as OpenAI,
so the setting keeps OpenAI's name.

### 5. Start it

```sh
cargo run --release
```

The first run downloads and compiles everything, which takes a few minutes. Later runs start in
seconds. When it prints that it is listening, open <http://127.0.0.1:8080>, tap the orb, allow
the microphone when the browser asks, and start talking.

To stop it, press Ctrl+C in the terminal.

### If something goes wrong

| What you see | What it means |
|---|---|
| `cargo: command not found` | Rust is not installed, or the terminal was open before you installed it. Open a new terminal |
| Stuck on "Connecting…" | Another program is using port 8080, or the microphone was blocked |
| The browser never asks for the microphone | Use `127.0.0.1`, not your computer's name or IP — browsers only allow microphones on localhost or HTTPS |
| `Deepgram connect failed … 401` | The Deepgram key is wrong, or its free credit has run out |
| `returned 404 … model` in the terminal | The model name in `.env` is not one your account can use |

**No API keys yet?** Fake versions of all three services ship with the project, so you can try
the whole thing offline — see [Running without API keys](#running-without-api-keys).

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

## The web page

The whole client is **one file**, `web/index.html` — about 700 lines of plain HTML, CSS and
JavaScript. No npm, no React, no build step, nothing to install. In a release build it is
compiled into the binary, so the server has no static files to serve or lose.

What it does:

- **A voice orb that reacts to the real audio.** Web Audio analysers measure your microphone and
  the agent's voice, and the orb's glow, scale and colour follow whoever is talking. Seven level
  bars below it follow the voice spectrum.
- **It shows the state of the call** — idle, connecting, listening, hearing you, speaking —
  each with its own palette, so you can tell what the agent is doing without reading anything.
- **Settings behind the gear**, saved in your browser and locked while a call is live.
- **It notices when the server hangs up.** The server sends audio every 20 ms, silence included,
  so two seconds without a packet means the call is over — much sooner than the browser reports
  it.

To change how it looks, edit the file and refresh the browser: in a dev build the page is read
from disk on every request, so there is nothing to rebuild.

## Running without API keys

Mock versions of Deepgram, the model and Murf ship in `examples/`, so the pipeline runs with no
account anywhere. Start each in its own terminal:

```sh
cargo run --example mock_deepgram
cargo run --example mock_openai
cargo run --example mock_murf
```

Then uncomment the `DEEPGRAM_URL`, `OPENAI_BASE_URL` and `MURF_URL` lines at the bottom of your
`.env` and start the server. It transcribes, replies and speaks in tones — enough to see the
orb, the interruptions and the latency logs working.

## Developing

- **Use plain `cargo run`** while working on it: it links faster, and it serves `web/index.html`
  from disk, so page edits need only a browser refresh. `--release` is for actually talking to
  it, since audio encoding is much slower in a debug build.
- **Rust changes need a restart**, and on Windows the server must be stopped before it can be
  rebuilt — the running `.exe` cannot be replaced while it is open.
- `cargo test` and `cargo clippy --all-targets` before a PR.

Each stage lives in its own module: `src/stt.rs` (speech to text), `src/llm.rs` (the model),
`src/tts.rs` (the voice), `src/agent.rs` (turns and barge-in), `src/session.rs` (one WebRTC
call). `PLAN.md` has the design decisions and `WORKLOG.md` a dated record of the work.

## Deploying

`cargo build --release` produces one self-contained binary with the web page built in, or use
the `Dockerfile`. For a public deployment:

- Put it behind HTTPS — browsers refuse microphone access otherwise
- Set `ACCESS_TOKEN`, or anyone who finds the URL spends your credit
- Set `PUBLIC_IP` and open the `RTC_PORT_MIN`–`RTC_PORT_MAX` **UDP** range: call audio goes
  straight to the server, not through the reverse proxy
- Size `MAX_CALLS` and `MAX_CALL_SECS` to your budget

Every setting is documented in `.env.example`.

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
  server — and no frontend build either: the web page is a single HTML file compiled into the
  binary. Copy one file and run it.
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

Issues and pull requests are welcome — new providers especially. A new one does not need to
touch the rest of the pipeline; see [Developing](#developing). Never commit your `.env` file:
it holds your keys, and the project ignores it for that reason.

## License

[MIT](LICENSE)
