//! A backend that streams canned text, with a configurable delay between
//! tokens.
//!
//! Not a test fixture that leaked into the library: the walking skeleton has to
//! be runnable, and its behaviour observable, without a model on the machine.
//! It is also the only way to exercise the slow-generation path deterministically
//! — which is exactly where a UI that re-renders per token falls over.

use std::time::Duration;

use super::{Backend, BackendError, Chunk, ChunkStream, CompletionRequest, StopReason, Usage};

pub struct Mock {
    /// One per call, in order; the last one repeats once they run out, unless
    /// `cycling` wraps back to the first instead. A single reply is the
    /// ordinary case and a list is what makes the tool loop runnable without
    /// a model — a scripted call, then the answer to its result.
    replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    cycling: bool,
    delay: Duration,
    fail_with: Option<String>,
}

impl Mock {
    pub fn new(reply: impl Into<String>) -> Self {
        Self::replies(vec![reply.into()])
    }

    pub fn replies(replies: Vec<String>) -> Self {
        Self {
            replies: std::sync::Mutex::new(replies.into()),
            cycling: false,
            delay: Duration::from_millis(25),
            fail_with: None,
        }
    }

    /// Off by default, so every existing script — one reply that repeats, or
    /// a call-then-answer pair that answers forever after the first tool
    /// loop — keeps meaning what it always meant. On, the list wraps back to
    /// its first element instead of sticking on its last, which is how a
    /// `call, answer` pair becomes a tool invoked on every turn of a script
    /// of any length, rather than once at the start of the session.
    pub fn cycle(mut self, cycling: bool) -> Self {
        self.cycling = cycling;
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Makes the backend fail partway, so the error path is reachable on demand.
    pub fn failing(mut self, message: impl Into<String>) -> Self {
        self.fail_with = Some(message.into());
        self
    }
}

impl Default for Mock {
    fn default() -> Self {
        Self::new(
            "This is the mock backend. It streams a fixed reply one word at a \
             time so the agent loop, the protocol and the debug UI can be run \
             without a model on the machine.",
        )
    }
}

impl Backend for Mock {
    fn name(&self) -> &str {
        "mock"
    }

    /// The one name it answers to, so a picker over a mock profile is not empty.
    fn models(&self) -> super::BackendFuture<'_, Vec<String>> {
        Box::pin(async { Ok(vec!["mock".to_string()]) })
    }

    /// `stream` below takes `_request` and reads nothing from it — the mock
    /// cannot enforce a schema or a grammar any more than it can enforce
    /// the window, so a `--constrain` run against it must not look like an
    /// unconstrained one that happened to pass, the same rule the window
    /// caveat set.
    fn constrain_caveat(&self, _constraint: &super::Constraint) -> Option<String> {
        Some("the mock backend does not enforce constraints; this run is unconstrained".into())
    }

    fn stream(&self, _request: CompletionRequest) -> ChunkStream<'_> {
        let reply = {
            let mut replies = self.replies.lock().expect("no panic holds this lock");
            match self.cycling {
                true => match replies.pop_front() {
                    Some(front) => {
                        replies.push_back(front.clone());
                        front
                    }
                    None => String::new(),
                },
                false => match replies.len() > 1 {
                    true => replies.pop_front().unwrap_or_default(),
                    false => replies.front().cloned().unwrap_or_default(),
                },
            }
        };
        // Word by word, because what this backend exists to exercise is a
        // stream arriving in pieces.
        let words: Vec<String> = reply.split_inclusive(' ').map(str::to_string).collect();
        let delay = self.delay;
        let fail_with = self.fail_with.clone();

        Box::pin(async_stream::try_stream! {
            let completion_tokens = words.len() as u32;

            for (i, word) in words.into_iter().enumerate() {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                // Same macro-expansion limit as in `ollama.rs`: no let chains here.
                let failing = fail_with.as_ref().filter(|_| i == completion_tokens as usize / 2);
                if let Some(message) = failing {
                    Err(BackendError::Transport(message.clone()))?;
                    return;
                }
                yield Chunk::Text(word);
            }

            yield Chunk::Done {
                stop: StopReason::Stop,
                // `Some`, and the prompt count is honestly zero: the mock never
                // read a prompt. That is not the same as a backend that read
                // one and did not say how big it was, which is `None`.
                usage: Some(Usage { prompt_tokens: 0, completion_tokens }),
            };
        })
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;
    use crate::backend::Message;

    fn request() -> CompletionRequest {
        CompletionRequest {
            tools: Vec::new(),
            model: "mock".into(),
            messages: vec![Message::user("hola")],
            context_limit: None,
            temperature: None,
            seed: None,
            constraint: None,
        }
    }

    async fn text_of(mock: &Mock) -> String {
        let mut stream = mock.stream(request());
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            if let Chunk::Text(word) = chunk.expect("the mock never fails without .failing()") {
                text.push_str(&word);
            }
        }
        text
    }

    #[tokio::test]
    async fn off_by_default_the_last_reply_repeats() {
        let mock = Mock::replies(vec!["call".into(), "answer".into()]).delay(Duration::ZERO);
        assert_eq!(text_of(&mock).await, "call");
        assert_eq!(text_of(&mock).await, "answer");
        assert_eq!(text_of(&mock).await, "answer");
        assert_eq!(text_of(&mock).await, "answer");
    }

    #[tokio::test]
    async fn cycling_wraps_back_to_the_first_instead_of_sticking_on_the_last() {
        let mock = Mock::replies(vec!["call".into(), "answer".into()])
            .cycle(true)
            .delay(Duration::ZERO);
        assert_eq!(text_of(&mock).await, "call");
        assert_eq!(text_of(&mock).await, "answer");
        assert_eq!(text_of(&mock).await, "call");
        assert_eq!(text_of(&mock).await, "answer");
        assert_eq!(text_of(&mock).await, "call");
    }

    #[tokio::test]
    async fn cycling_one_reply_is_the_same_as_not_cycling() {
        let mock = Mock::new("only").cycle(true).delay(Duration::ZERO);
        assert_eq!(text_of(&mock).await, "only");
        assert_eq!(text_of(&mock).await, "only");
    }
}
