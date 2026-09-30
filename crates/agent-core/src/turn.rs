//! One turn: prompt in, tokens out.
//!
//! No tools and no context management yet — this is the walking skeleton's
//! middle, and its job is to make the events real before they are written down
//! as a wire protocol (`RECORD/2026-08-26.walking-skeleton.completed.md`).

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};

use crate::backend::{Backend, Chunk, CompletionRequest, Message, StopReason, Usage};
use crate::tools::{ToolCall, ToolStep};

/// Why a turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Stop,
    Length,
    Other,
    /// The turn used its whole tool budget without arriving at an answer.
    /// Distinct from every other reason because the text that came back is an
    /// investigation cut short, not a conclusion — and a client that cannot
    /// tell will present it as one.
    ToolLimit,
    /// The caller cancelled. Distinct from every model-side reason, because
    /// only this one means the answer is incomplete by our own doing.
    Cancelled,
}

impl From<StopReason> for EndReason {
    fn from(stop: StopReason) -> Self {
        match stop {
            StopReason::Stop => Self::Stop,
            StopReason::Length => Self::Length,
            StopReason::Other => Self::Other,
        }
    }
}

/// What the loop emits as it runs. The wire protocol is derived from this, not
/// the other way round.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    Token(String),
    /// A call to the model, announced before it is made, with the exact
    /// messages it sends. `step` counts from 1 within the turn.
    ///
    /// A turn that uses a tool is several calls, not one — the first ends in a
    /// tool block, the next reads the result — and the counts the backend
    /// reports on `Ended` are summed over all of them. Announcing every call is
    /// what lets the trace channel measure the ones after the first; without it
    /// our count and the backend's are not counts of the same thing, and
    /// nothing says so. Debug data, so it never becomes a protocol message.
    ///
    /// It costs one clone of the prompt per call, made whether or not anyone is
    /// recording — the same order as the clone the request already needs.
    ///
    /// `retry` is `true` for the one extra call `SchemaRetry` spends on a
    /// drifted reply, same `step` as the attempt it is retrying. Before this
    /// field existed a retry's own `ModelCall` was indistinguishable from a
    /// first attempt's, both carrying `step == 1` — which is exactly why the
    /// interceptor's old `step > 1` rule to measure a `StepCall` missed the
    /// retry outright, a gap named in
    /// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`'s "the tool-call
    /// probe's own instrument cannot see a retry".
    ModelCall {
        step: u32,
        messages: Vec<Message>,
        retry: bool,
    },
    /// The model asked for a tool. Emitted before the sandbox is consulted, so
    /// a denial is visible as a call that was made and refused rather than as
    /// nothing happening.
    ToolCall {
        step: u32,
        call: ToolCall,
    },
    /// What it did, including the verdict and who enforced it.
    ToolResult {
        step: u32,
        outcome: Box<ToolStep>,
    },
    /// `usage` is absent on a cancelled turn: the counts arrive on the
    /// backend's final line, and cancelling means never reading it.
    Ended {
        reason: EndReason,
        usage: Option<Usage>,
    },
    Failed(String),
    /// `run_agent_turn`'s schema retry was refused by the backend — a grammar
    /// or schema it would not compile, a transport error, whatever it said no
    /// to. The turn survives by falling back to the pre-retry answer
    /// (`RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`'s "what a refused
    /// retry does to a turn"); this is the loud, once-per-occurrence note that
    /// fallback needed and did not have. Debug data, like `ModelCall`: it
    /// explains the agent rather than driving it, and never becomes a
    /// protocol message.
    ConstraintRefused {
        error: String,
    },
}

/// What the caller gets back, once the turn is over.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// Everything the model produced, assembled. Partial on a cancel or a
    /// failure — the caller decides whether a partial answer is worth keeping.
    pub text: String,
    pub reason: EndReason,
    pub usage: Option<Usage>,
    pub error: Option<String>,
}

/// Runs one turn to completion, emitting events as they happen.
///
/// Cancellation is checked between chunks, so it takes effect at the next token
/// rather than mid-token. Dropping `events` does not stop the turn: the loop
/// keeps draining the backend so the outcome stays complete.
pub async fn run_turn(
    backend: &dyn Backend,
    request: CompletionRequest,
    events: mpsc::Sender<TurnEvent>,
    mut cancel: watch::Receiver<bool>,
) -> TurnOutcome {
    let mut text = String::new();
    let mut stream = backend.stream(request);

    macro_rules! emit {
        ($event:expr) => {
            let _ = events.send($event).await;
        };
    }

    if *cancel.borrow_and_update() {
        emit!(TurnEvent::Ended {
            reason: EndReason::Cancelled,
            usage: None
        });
        return TurnOutcome {
            text,
            reason: EndReason::Cancelled,
            usage: None,
            error: None,
        };
    }

    // Every cancel sender may be dropped before the turn ends — that is the
    // ordinary shape of a caller with nothing to cancel with. Once it happens,
    // `changed()` is permanently ready with an error, so it has to stop being
    // selected on, or the biased arm starves the stream forever.
    let mut cancellable = true;

    loop {
        let next = if cancellable {
            tokio::select! {
                biased;
                changed = cancel.changed() => {
                    match changed {
                        Ok(()) if *cancel.borrow() => {
                            emit!(TurnEvent::Ended { reason: EndReason::Cancelled, usage: None });
                            return TurnOutcome {
                                text,
                                reason: EndReason::Cancelled,
                                usage: None,
                                error: None,
                            };
                        }
                        Ok(()) => continue,
                        Err(_) => {
                            cancellable = false;
                            continue;
                        }
                    }
                }
                next = stream.next() => next,
            }
        } else {
            stream.next().await
        };

        match next {
            Some(Ok(Chunk::Text(delta))) => {
                text.push_str(&delta);
                emit!(TurnEvent::Token(delta));
            }
            Some(Ok(Chunk::Done { stop, usage })) => {
                let reason = EndReason::from(stop);
                // Passed through as it arrived: a backend that did not report
                // usage says `None` here, and turning that into zeros would be
                // the one lie this field cannot afford.
                emit!(TurnEvent::Ended { reason, usage });
                return TurnOutcome {
                    text,
                    reason,
                    usage,
                    error: None,
                };
            }
            Some(Err(error)) => {
                let error = error.to_string();
                emit!(TurnEvent::Failed(error.clone()));
                return TurnOutcome {
                    text,
                    reason: EndReason::Other,
                    usage: None,
                    error: Some(error),
                };
            }
            // The backend's stream ended without saying so. Treat it as a stop,
            // and say the usage is unknown rather than reporting zeros.
            None => {
                emit!(TurnEvent::Ended {
                    reason: EndReason::Stop,
                    usage: None
                });
                return TurnOutcome {
                    text,
                    reason: EndReason::Stop,
                    usage: None,
                    error: None,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::backend::{Message, mock::Mock};

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

    async fn collect(
        backend: &dyn Backend,
        cancel: watch::Receiver<bool>,
    ) -> (Vec<TurnEvent>, TurnOutcome) {
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(event) = rx.recv().await {
                seen.push(event);
            }
            seen
        });
        let outcome = run_turn(backend, request(), tx, cancel).await;
        (drain.await.unwrap(), outcome)
    }

    #[tokio::test]
    async fn a_turn_streams_its_tokens_then_ends() {
        let backend = Mock::new("uno dos tres").delay(Duration::ZERO);
        let (_, never) = watch::channel(false);
        let (events, outcome) = collect(&backend, never).await;

        let tokens: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TurnEvent::Token(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tokens, ["uno ", "dos ", "tres"]);
        assert_eq!(outcome.text, "uno dos tres");
        assert_eq!(outcome.reason, EndReason::Stop);
        assert_eq!(outcome.usage.map(|u| u.completion_tokens), Some(3));
        assert!(matches!(events.last(), Some(TurnEvent::Ended { .. })));
    }

    #[tokio::test]
    async fn cancelling_ends_the_turn_and_keeps_the_partial_text() {
        let backend = Mock::new("uno dos tres cuatro").delay(Duration::from_millis(30));
        let (stop, cancel) = watch::channel(false);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(45)).await;
            let _ = stop.send(true);
        });

        let (events, outcome) = collect(&backend, cancel).await;

        assert_eq!(outcome.reason, EndReason::Cancelled);
        // Cancelling means the backend's final line is never read, so there is
        // no usage to report — the one thing this test exists to pin down.
        assert!(outcome.usage.is_none());
        assert!(
            !outcome.text.is_empty(),
            "the tokens already streamed are kept"
        );
        assert!(outcome.text.len() < "uno dos tres cuatro".len());
        assert!(matches!(
            events.last(),
            Some(TurnEvent::Ended {
                reason: EndReason::Cancelled,
                usage: None
            })
        ));
    }

    #[tokio::test]
    async fn cancelling_before_the_first_token_still_ends_cleanly() {
        let backend = Mock::new("uno dos").delay(Duration::from_millis(50));
        let (_stop, cancel) = watch::channel(true);
        let (events, outcome) = collect(&backend, cancel).await;

        assert_eq!(outcome.reason, EndReason::Cancelled);
        assert!(outcome.text.is_empty());
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn dropping_the_cancel_sender_does_not_stall_the_turn() {
        // Regression: with every sender gone, `watch::Receiver::changed()` is
        // permanently ready with an error. A biased select that kept polling it
        // never reached the backend stream, and the turn hung.
        let backend = Mock::new("uno dos").delay(Duration::ZERO);
        let (sender, cancel) = watch::channel(false);
        drop(sender);

        let (_, outcome) = tokio::time::timeout(Duration::from_secs(5), collect(&backend, cancel))
            .await
            .expect("the turn must finish once cancellation becomes impossible");

        assert_eq!(outcome.reason, EndReason::Stop);
        assert_eq!(outcome.text, "uno dos");
    }

    #[tokio::test]
    async fn a_backend_failure_is_reported_and_keeps_what_arrived() {
        let backend = Mock::new("uno dos tres cuatro")
            .delay(Duration::ZERO)
            .failing("connection reset");
        let (_, never) = watch::channel(false);
        let (events, outcome) = collect(&backend, never).await;

        assert!(
            outcome
                .error
                .as_deref()
                .unwrap()
                .contains("connection reset")
        );
        assert!(!outcome.text.is_empty());
        assert!(matches!(events.last(), Some(TurnEvent::Failed(_))));
    }
}
