//! Splits a stream of language-model tokens into speakable sentences.
//!
//! Speech starts as soon as the first sentence is complete rather than waiting for the whole
//! reply, which is most of the difference between a snappy agent and a sluggish one.

/// Flush at a sentence end, or at this many characters if the model rambles on.
const MAX_CHARS: usize = 120;

#[derive(Default)]
pub struct Chunker {
    pending: String,
}

impl Chunker {
    /// Adds a token, returning a sentence once one is complete.
    pub fn push(&mut self, token: &str) -> Option<String> {
        self.pending.push_str(token);

        let ends_sentence = self
            .pending
            .trim_end()
            .ends_with(['.', '!', '?', '\n', ':', ';']);
        // "3.5" and "Dr. Who" would split mid-sentence, so require a following space.
        let settled = token.ends_with([' ', '\n']) || token.is_empty();

        if (ends_sentence && settled) || self.pending.len() >= MAX_CHARS {
            return self.take();
        }
        None
    }

    /// Whatever is left when the reply ends.
    pub fn flush(&mut self) -> Option<String> {
        self.take()
    }

    fn take(&mut self) -> Option<String> {
        let sentence = self.pending.trim().to_owned();
        self.pending.clear();
        (!sentence.is_empty()).then_some(sentence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds tokens the way a model emits them: fragments, spaces attached.
    fn sentences(tokens: &[&str]) -> Vec<String> {
        let mut chunker = Chunker::default();
        let mut out: Vec<String> = tokens.iter().filter_map(|t| chunker.push(t)).collect();
        out.extend(chunker.flush());
        out
    }

    #[test]
    fn a_sentence_is_emitted_as_soon_as_it_ends() {
        assert_eq!(
            sentences(&["Hello ", "there. ", "How ", "are ", "you?"]),
            ["Hello there.", "How are you?"]
        );
    }

    #[test]
    fn a_decimal_point_does_not_split_a_sentence() {
        assert_eq!(sentences(&["It ", "is ", "3.", "5 ", "degrees."]), ["It is 3.5 degrees."]);
    }

    #[test]
    fn a_long_ramble_is_flushed_before_it_gets_too_long() {
        let long = "word ".repeat(40);
        let out = sentences(&[&long]);
        assert!(!out.is_empty());
        assert!(out[0].len() >= MAX_CHARS - 5, "{}", out[0].len());
    }

    #[test]
    fn trailing_text_without_punctuation_is_still_spoken() {
        assert_eq!(sentences(&["no ", "full ", "stop"]), ["no full stop"]);
    }
}
