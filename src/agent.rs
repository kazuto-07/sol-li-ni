//! The conversation brain: transcribe the caller, answer with the language model, speak the
//! answer — and stop all of it the moment the caller talks again.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::task::AbortOnDrop;
use tracing::{debug, info, warn};

use crate::audio::{Frame, Speaker};
use crate::chunker::Chunker;
use crate::llm::{Llm, Message};
use crate::metrics::TurnMetrics;
use crate::stt::{Session, SttEvent};
use crate::tts::{self, Command, Speech};

/// Messages kept besides the system prompt. Every turn resends the history, so an unbounded one
/// makes long calls slower and costlier per reply; twenty exchanges is plenty of context for a
/// spoken conversation.
const MAX_HISTORY: usize = 40;

pub struct AgentIo {
    /// Decoded caller audio, 20 ms mono frames.
    pub mic: mpsc::Receiver<Frame>,
    /// Audio to send to the caller. Drained by the 20 ms pacer, flushed on barge-in.
    pub speaker: Arc<Speaker>,
}

/// A reply in progress: the task producing it, and what has been said so far.
struct Turn {
    task: AbortOnDrop<()>,
    context_id: String,
    /// Text handed to the voice. On an interruption this is roughly what the caller heard.
    spoken: Arc<Mutex<String>>,
}

/// `stt` and `tts` are the sessions warmed up while the call was connecting.
pub async fn run(
    llm: Llm,
    system_prompt: String,
    stt: JoinHandle<anyhow::Result<Session>>,
    tts: JoinHandle<anyhow::Result<tts::Session>>,
    io: AgentIo,
    metrics: Arc<Mutex<TurnMetrics>>,
) {
    // Owned from here on: if this agent is aborted while still waiting on them, the warm-up
    // sockets close with it.
    let mut stt = AbortOnDrop::new(stt);
    let mut tts = AbortOnDrop::new(tts);

    let session = match stt.join().await {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => return warn!("stt unavailable: {e:#}"),
        Err(e) => return warn!("stt warmup task failed: {e}"),
    };

    let (events_tx, mut events_rx) = mpsc::channel(50);
    let _transcribe = AbortOnDrop::new(tokio::spawn(async move {
        if let Err(e) = session.run(io.mic, events_tx).await {
            warn!("stt: {e:#}");
        }
    }));

    // The voice runs as its own task: commands in, audio straight into the speaker queue.
    let (speech_tx, speech_rx) = mpsc::channel(50);
    let _speaking = match tts.join().await {
        Ok(Ok(voice)) => {
            let speaker = Arc::clone(&io.speaker);
            Some(AbortOnDrop::new(tokio::spawn(async move {
                if let Err(e) = voice.run(speech_rx, speaker).await {
                    warn!("tts: {e:#}");
                }
            })))
        }
        Ok(Err(e)) => {
            warn!("tts unavailable, replies will not be spoken: {e:#}");
            None
        }
        Err(e) => {
            warn!("tts warmup task failed: {e}");
            None
        }
    };

    let history = Arc::new(Mutex::new(vec![Message::system(system_prompt)]));

    // Finals arrive segment by segment; the turn is whatever was said before the caller stopped.
    let mut turn = String::new();
    let mut in_flight: Option<Turn> = None;

    while let Some(event) = events_rx.recv().await {
        match event {
            SttEvent::SpeechStarted => debug!("speech started"),

            SttEvent::Interim { text, lag } => {
                metrics.lock().expect("metrics lock").interim(lag);
                info!("… {text}");
                // Words while the agent is talking mean it has been interrupted.
                interrupt(&mut in_flight, &io.speaker, &speech_tx, &history, &metrics).await;
            }

            SttEvent::Final { text, lag } => {
                metrics.lock().expect("metrics lock").transcript_final(lag);
                if !turn.is_empty() {
                    turn.push(' ');
                }
                turn.push_str(&text);
            }

            SttEvent::UtteranceEnd => {
                if turn.is_empty() {
                    continue;
                }
                // Anything still running belongs to a turn the caller has moved past.
                interrupt(&mut in_flight, &io.speaker, &speech_tx, &history, &metrics).await;

                let said = std::mem::take(&mut turn);
                info!("USER: {said}");
                metrics.lock().expect("metrics lock").utterance_end();

                let mut messages = history.lock().expect("history lock");
                messages.push(Message::user(said));
                trim_history(&mut messages);
                let context_id = format!("turn-{}", messages.len());
                let snapshot = messages.clone();
                drop(messages);

                // The reply runs as its own task so this loop stays free to notice the caller
                // speaking over it.
                let spoken = Arc::new(Mutex::new(String::new()));
                in_flight = Some(Turn {
                    task: AbortOnDrop::new(tokio::spawn(answer(
                        llm.clone(),
                        snapshot,
                        speech_tx.clone(),
                        Arc::clone(&metrics),
                        Arc::clone(&history),
                        context_id.clone(),
                        Arc::clone(&spoken),
                    ))),
                    context_id,
                    spoken,
                });
            }
        }
    }

    // The in-flight turn, transcriber and voice are all guarded and stop as this returns.
}

/// Keeps the system prompt and the most recent `MAX_HISTORY` messages.
fn trim_history(messages: &mut Vec<Message>) {
    let excess = messages.len().saturating_sub(MAX_HISTORY + 1);
    if excess > 0 {
        messages.drain(1..=excess);
    }
}

/// Stops a reply that is still streaming or speaking, and remembers only what was heard.
async fn interrupt(
    in_flight: &mut Option<Turn>,
    speaker: &Speaker,
    speech: &mpsc::Sender<Command>,
    history: &Mutex<Vec<Message>>,
    metrics: &Mutex<TurnMetrics>,
) {
    let Some(turn) = in_flight.take() else { return };

    // A finished task does not mean a finished reply: the audio it produced is usually still
    // queued and playing, so the queue is flushed either way.
    let streaming = !turn.task.is_finished();
    if streaming {
        turn.task.abort();
    }
    let dropped = speaker.clear();
    if !streaming && dropped.is_zero() {
        return;
    }
    let _ = speech.send(Command::Clear { context_id: turn.context_id }).await;

    // Only a cut-off reply needs recording here; a completed one has already been added, and
    // what the caller heard is what was handed to the voice minus what was still queued.
    if streaming {
        let spoken = turn.spoken.lock().expect("spoken lock").clone();
        if !spoken.is_empty() {
            history.lock().expect("history lock").push(Message::assistant(spoken));
        }
    }
    metrics.lock().expect("metrics lock").interrupted(dropped);
}

/// Streams one reply, speaking each sentence as soon as it is complete.
#[allow(clippy::too_many_arguments, reason = "a turn genuinely carries all of this")]
async fn answer(
    llm: Llm,
    snapshot: Vec<Message>,
    speech: mpsc::Sender<Command>,
    metrics: Arc<Mutex<TurnMetrics>>,
    history: Arc<Mutex<Vec<Message>>>,
    context_id: String,
    spoken: Arc<Mutex<String>>,
) {
    let mut stream = match llm.stream(&snapshot).await {
        Ok(stream) => Box::pin(stream),
        Err(e) => {
            warn!("llm: {e:#}");
            // Leave no user turn in the history that the model never answered.
            history.lock().expect("history lock").pop();
            return;
        }
    };

    let mut chunker = Chunker::default();
    let mut reply = String::new();

    while let Some(token) = stream.next().await {
        let token = match token {
            Ok(token) => token,
            Err(e) => {
                warn!("llm: {e:#}");
                break;
            }
        };
        if reply.is_empty() {
            metrics.lock().expect("metrics lock").llm_first_token();
        }
        reply.push_str(&token);
        if let Some(sentence) = chunker.push(&token) {
            say(&speech, &context_id, &spoken, sentence, false).await;
        }
    }

    // The final message closes the context, even when there is nothing left to say.
    say(&speech, &context_id, &spoken, chunker.flush().unwrap_or_default(), true).await;

    info!("AGENT: {reply}");
    metrics.lock().expect("metrics lock").llm_done();
    if !reply.is_empty() {
        history.lock().expect("history lock").push(Message::assistant(reply));
    }
}

async fn say(
    speech: &mpsc::Sender<Command>,
    context_id: &str,
    spoken: &Mutex<String>,
    text: String,
    end: bool,
) {
    debug!("speaking {text:?} (end: {end})");
    if !text.is_empty() {
        let mut said = spoken.lock().expect("spoken lock");
        if !said.is_empty() {
            said.push(' ');
        }
        said.push_str(&text);
    }
    let _ = speech
        .send(Command::Speak(Speech { text, context_id: context_id.to_owned(), end }))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_the_system_prompt_and_the_latest_messages() {
        let mut messages = vec![Message::system("be brief")];
        for i in 0..MAX_HISTORY + 10 {
            messages.push(Message::user(format!("turn {i}")));
        }
        trim_history(&mut messages);

        assert_eq!(messages.len(), MAX_HISTORY + 1);
        assert_eq!(messages[0].role, "system", "the system prompt must survive trimming");
        assert_eq!(messages[1].content, "turn 10", "the oldest turns go first");
        assert_eq!(messages.last().unwrap().content, format!("turn {}", MAX_HISTORY + 9));
    }

    #[test]
    fn a_short_history_is_left_alone() {
        let mut messages = vec![Message::system("be brief"), Message::user("hi")];
        trim_history(&mut messages);
        assert_eq!(messages.len(), 2);
    }
}
