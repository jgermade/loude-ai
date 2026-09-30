//! The loop: prompt, model, tool, model, answer.
//!
//! ```text
//! prompt ─► model ─► text with a tool call in it?
//!                      │ no  → the turn's answer
//!                      │ yes → parse → check the policy → execute → append → model
//! ```
//!
//! [`run_turn`] is the model half and this is the loop around it, rather than
//! one function doing both: cancellation, streaming and the backend's stop
//! reasons are already right there, and a second implementation of them would
//! be a second set of bugs.
//!
//! Every step is a real message pair — `assistant(call)` then `user(result)` —
//! so the strict alternation the prompt shape depends on holds all the way
//! through, and the turn that gets stored can be replayed exactly.
//!
//! See `RECORD/2026-08-27.tools-and-sandbox.completed.md`.

use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::backend::{Backend, CompletionRequest, Constraint, Message, Usage};
use crate::sandbox::Sandbox;
use crate::tools::{CallVerdict, ToolStep, parse_call, score_call};
use crate::turn::{EndReason, TurnEvent, TurnOutcome, run_turn};
use crate::worker::Executor;

/// The retry `Constraint::Schema` is honest for — see
/// [`crate::tools::Tools::call_schema`]'s own doc and
/// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`. An unconstrained
/// first attempt keeps "just answer" reachable; only a reply that attempted
/// a call and missed the shape (`CallVerdict::Drifted`) is retried, once,
/// forced into this schema. A clean decline (`CallVerdict::NoCall`) is never
/// retried — a schema cannot tell "the model chose not to call" from "the
/// model forgot to," so the loop does not guess at the ones score_call
/// already can tell apart.
#[derive(Debug, Clone)]
pub struct SchemaRetry {
    pub schema: serde_json::Value,
    pub tool_names: Vec<&'static str>,
}

/// How many tool calls one turn may make before it has to answer.
///
/// A default, not a law. Too low and the agent cannot finish an investigation;
/// too high and a model that has decided to `list_dir` forever costs a session.
pub const DEFAULT_MAX_STEPS: u32 = 8;

/// What bounds a turn: how many calls it may make, and how long any one of them
/// may take.
///
/// Two numbers in one struct because they answer the same question — *what
/// stops this turn from running forever* — and because a loop that took one and
/// not the other could still be stopped by neither.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub max_steps: u32,
    /// Added to whatever clock the call carries of its own
    /// ([`crate::tools::command::clock_of`]), which is zero for every tool
    /// except `run_command`. **The loop holds this, not the seam**: a container
    /// is one place tools can run and `Runtime::Host` is another, and only the
    /// loop is above both. See
    /// `RECORD/2026-09-05.a-clock-where-there-is-no-seam.completed.md`.
    pub tool_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            tool_timeout: Duration::from_millis(crate::worker::runtime::DEFAULT_TIMEOUT_MS),
        }
    }
}

impl Limits {
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    pub fn with_tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = timeout;
        self
    }

    /// How long this particular call may take. Never less than what the tool
    /// was itself told it could have: a deadline that pre-empted `run_command`'s
    /// own timeout would kill a command still inside the budget it was given.
    fn deadline(&self, call: &crate::tools::ToolCall) -> Duration {
        crate::tools::command::clock_of(call) + self.tool_timeout
    }
}

/// What a turn with tools produced.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    /// The final assistant text — what the model said once it stopped calling
    /// tools. Partial on a cancel or a failure.
    pub text: String,
    /// What it did on the way, in order, ready to be stored with the turn.
    pub steps: Vec<ToolStep>,
    pub reason: EndReason,
    /// Summed over the model calls this turn made. The prompt was sent once per
    /// step, so this double-counts the shared prefix on purpose: it is what the
    /// backend actually processed, and it is the cost the turn paid.
    pub usage: Option<Usage>,
    pub error: Option<String>,
}

impl AgentOutcome {
    fn from_final(
        text: String,
        steps: Vec<ToolStep>,
        last: TurnOutcome,
        usage: Option<Usage>,
    ) -> Self {
        Self {
            text,
            steps,
            reason: last.reason,
            usage,
            error: last.error,
        }
    }
}

/// Runs one turn, letting the model use tools until it answers.
///
/// `request.messages` is the prompt as the context manager assembled it; the
/// steps are appended to a copy, so the caller's selection is not rewritten
/// underneath it.
///
/// `tools` is an [`Executor`] rather than a [`crate::tools::Tools`] because
/// **where** a call runs is not this loop's business: in this process, or down
/// a pipe into a container. That is the whole of what level 3 changed here —
/// and if adding the container had had to touch this function, the loop was
/// wrong. See `RECORD/2026-09-02.the-worker-and-the-seam.completed.md`.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_turn(
    backend: &dyn Backend,
    request: CompletionRequest,
    tools: &dyn Executor,
    sandbox: &Sandbox,
    limits: Limits,
    schema_retry: Option<&SchemaRetry>,
    events: mpsc::Sender<TurnEvent>,
    cancel: watch::Receiver<bool>,
) -> AgentOutcome {
    let CompletionRequest {
        model,
        mut messages,
        context_limit,
        temperature,
        seed,
        constraint,
        tools: specs,
    } = request;
    let mut steps: Vec<ToolStep> = Vec::new();
    let mut usage: Option<Usage> = None;

    for step in 1..=limits.max_steps.max(1) {
        // Tokens are forwarded as they arrive; the intermediate `Ended` is not,
        // because a turn ends once and a client that saw three would draw three.
        let (inner, mut inbox) = mpsc::channel::<TurnEvent>(256);
        let forward = {
            let events = events.clone();
            tokio::spawn(async move {
                while let Some(event) = inbox.recv().await {
                    if let TurnEvent::Token(_) = event
                        && events.send(event).await.is_err()
                    {
                        break;
                    }
                }
            })
        };

        // Before the call, and including the first: the loop announces every
        // call it makes, and what to do with that is the caller's business.
        let _ = events
            .send(TurnEvent::ModelCall {
                step,
                messages: messages.clone(),
                retry: false,
            })
            .await;

        let outcome = run_turn(
            backend,
            CompletionRequest {
                tools: specs.clone(),
                model: model.clone(),
                messages: messages.clone(),
                // Every call of the turn budgets against the same window, so
                // every call has to be told the same one.
                context_limit,
                temperature,
                seed,
                constraint: constraint.clone(),
            },
            inner,
            cancel.clone(),
        )
        .await;
        let _ = forward.await;
        usage = sum(usage, outcome.usage);

        // A failed or cancelled step ends the turn where it is. Feeding a tool
        // call parsed out of a half-generated answer back into the loop would
        // execute something nobody finished asking for.
        if outcome.error.is_some() || outcome.reason == EndReason::Cancelled {
            let _ = events
                .send(match &outcome.error {
                    Some(error) => TurnEvent::Failed(error.clone()),
                    None => TurnEvent::Ended {
                        reason: EndReason::Cancelled,
                        usage: None,
                    },
                })
                .await;
            return AgentOutcome::from_final(outcome.text.clone(), steps, outcome, usage);
        }

        // The one retry `Constraint::Schema` is for: the first attempt above
        // was unconstrained, and only a `Drifted` verdict — a fenced block
        // under a tool's own name, the shape that missed rather than a
        // decision not to call — earns a second, schema-forced attempt. See
        // `SchemaRetry`'s own doc.
        let outcome = if parse_call(&outcome.text).is_none()
            && let Some(retry) = schema_retry
            && score_call(&outcome.text, &retry.tool_names) == CallVerdict::Drifted
        {
            let (inner, mut inbox) = mpsc::channel::<TurnEvent>(256);
            let forward = {
                let events = events.clone();
                tokio::spawn(async move {
                    while let Some(event) = inbox.recv().await {
                        if let TurnEvent::Token(_) = event
                            && events.send(event).await.is_err()
                        {
                            break;
                        }
                    }
                })
            };
            let _ = events
                .send(TurnEvent::ModelCall {
                    step,
                    messages: messages.clone(),
                    retry: true,
                })
                .await;
            let retried = run_turn(
                backend,
                CompletionRequest {
                    tools: specs.clone(),
                    model: model.clone(),
                    messages: messages.clone(),
                    context_limit,
                    temperature,
                    seed,
                    constraint: Some(Constraint::Schema(retry.schema.clone())),
                },
                inner,
                cancel.clone(),
            )
            .await;
            let _ = forward.await;
            usage = sum(usage, retried.usage);
            if retried.reason == EndReason::Cancelled {
                let _ = events
                    .send(TurnEvent::Ended {
                        reason: EndReason::Cancelled,
                        usage: None,
                    })
                    .await;
                return AgentOutcome::from_final(retried.text.clone(), steps, retried, usage);
            }
            match retried.error {
                // The retry itself was refused — a grammar or schema the
                // backend would not compile, a transport error, whatever it
                // said no to. The pre-retry attempt is a real reply the
                // model produced; throwing it away for an error would trade
                // a working (if malformed) answer for nothing. Fall back to
                // it, so a refused constraint degrades to "the drift the
                // retry couldn't fix" — which the ordinary `parse_call`
                // check below already knows how to end a turn on — instead
                // of failing the turn outright. See
                // `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`'s own
                // "what a refused grammar does to a session". Reported once,
                // here, the same rule `constrain_caveat` set for a known
                // incompatibility at startup — this is the runtime half of
                // it, discovered per compile rather than known in advance.
                Some(error) => {
                    let _ = events.send(TurnEvent::ConstraintRefused { error }).await;
                    outcome
                }
                None => retried,
            }
        } else {
            outcome
        };

        let Some(call) = parse_call(&outcome.text) else {
            let _ = events
                .send(TurnEvent::Ended {
                    reason: outcome.reason,
                    usage,
                })
                .await;
            return AgentOutcome::from_final(outcome.text.clone(), steps, outcome, usage);
        };

        // Emitted before the sandbox is consulted, so a denial reads as a call
        // that was refused rather than as nothing having happened.
        let _ = events
            .send(TurnEvent::ToolCall {
                step,
                call: call.clone(),
            })
            .await;

        let started = std::time::Instant::now();
        // The one clock over every tool call, wherever the call runs. A tool
        // that never answers used to hang the turn, the job and the session —
        // in a container it hung on a pipe, and under `Runtime::Host` it hung
        // on a syscall, which is the half that had nothing watching it at all.
        let deadline = limits.deadline(&call);
        let result = match tokio::time::timeout(deadline, tools.call(&call, sandbox)).await {
            Ok(result) => result,
            Err(_) => {
                // Dropping the future is what abandons the call; this is what
                // tells the executor to deal with what it left behind — a
                // worker process to kill, or nothing at all in this process.
                tools.abandon().await;
                let said = format!(
                    "`{}` did not answer in {} ms and was abandoned",
                    call.name,
                    deadline.as_millis(),
                );
                crate::tools::ToolOutcome::failed(crate::sandbox::Verdict::deny(said.clone()), said)
            }
        };
        let taken = ToolStep {
            text: outcome.text.clone(),
            call,
            outcome: result,
            duration_ms: started.elapsed().as_millis() as u64,
        };

        let _ = events
            .send(TurnEvent::ToolResult {
                step,
                outcome: Box::new(taken.clone()),
            })
            .await;

        messages.push(Message::assistant(taken.text.clone()));
        messages.push(Message::user(taken.result_text()));
        steps.push(taken);
    }

    // The budget is spent and the model is still working. Saying `stop` here
    // would present an investigation cut short as a conclusion.
    let text = steps
        .last()
        .map(|step| step.text.clone())
        .unwrap_or_default();
    let _ = events
        .send(TurnEvent::Ended {
            reason: EndReason::ToolLimit,
            usage,
        })
        .await;
    AgentOutcome {
        text,
        steps,
        reason: EndReason::ToolLimit,
        usage,
        error: None,
    }
}

/// Adds a step's counts to the turn's. Unknown plus known is known: a backend
/// that reported nothing on one step did not make the others unmeasured.
fn sum(total: Option<Usage>, step: Option<Usage>) -> Option<Usage> {
    match (total, step) {
        (Some(total), Some(step)) => Some(Usage {
            prompt_tokens: total.prompt_tokens + step.prompt_tokens,
            completion_tokens: total.completion_tokens + step.completion_tokens,
        }),
        (total, step) => total.or(step),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::backend::{BackendError, Chunk, ChunkStream, StopReason};
    use crate::sandbox::{Access, Applied, PathRule, SandboxPolicy};

    /// A backend that says one scripted thing per call, so a tool loop can be
    /// written down as the conversation it is meant to have.
    struct Scripted {
        replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl Scripted {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: std::sync::Mutex::new(
                    replies.iter().map(|text| (*text).to_string()).collect(),
                ),
            }
        }
    }

    impl Backend for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }

        fn stream(&self, _request: CompletionRequest) -> ChunkStream<'_> {
            let text = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "done".to_string());
            Box::pin(futures_util::stream::iter(vec![
                Ok(Chunk::Text(text)),
                Ok(Chunk::Done {
                    stop: StopReason::Stop,
                    usage: Some(Usage {
                        prompt_tokens: 10,
                        completion_tokens: 2,
                    }),
                }),
            ]))
        }
    }

    /// A backend whose first call drifts and whose second call — the
    /// schema-forced retry — is refused outright, the shape a grammar or
    /// schema the server won't compile takes.
    struct DriftThenRefuse {
        first_reply: String,
        calls: std::sync::atomic::AtomicU32,
    }

    impl DriftThenRefuse {
        fn new(first_reply: &str) -> Self {
            Self {
                first_reply: first_reply.to_string(),
                calls: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }

    impl Backend for DriftThenRefuse {
        fn name(&self) -> &str {
            "drift-then-refuse"
        }

        fn stream(&self, _request: CompletionRequest) -> ChunkStream<'_> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let text = self.first_reply.clone();
                return Box::pin(futures_util::stream::iter(vec![
                    Ok(Chunk::Text(text)),
                    Ok(Chunk::Done {
                        stop: StopReason::Stop,
                        usage: Some(Usage {
                            prompt_tokens: 10,
                            completion_tokens: 2,
                        }),
                    }),
                ]));
            }
            Box::pin(futures_util::stream::iter(vec![Err(
                BackendError::Rejected("failed to parse grammar".into()),
            )]))
        }
    }

    struct Fixture {
        root: PathBuf,
        sandbox: Sandbox,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "luu-agent-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("notes.txt"), "the answer is 42\n").unwrap();
            let root = root.canonicalize().unwrap();
            let sandbox = Sandbox::new(
                &SandboxPolicy {
                    paths: vec![PathRule::new(".", Access::ReadWrite)],
                    ..SandboxPolicy::default()
                },
                &root,
            )
            .unwrap();
            Self { root, sandbox }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    async fn drive(
        fixture: &Fixture,
        replies: &[&str],
        max_steps: u32,
    ) -> (Vec<TurnEvent>, AgentOutcome) {
        drive_with(fixture, replies, max_steps, None).await
    }

    async fn drive_with(
        fixture: &Fixture,
        replies: &[&str],
        max_steps: u32,
        schema_retry: Option<&SchemaRetry>,
    ) -> (Vec<TurnEvent>, AgentOutcome) {
        let backend = Scripted::new(replies);
        let tools = crate::tools::Tools::standard();
        let (tx, mut rx) = mpsc::channel(256);
        let drain = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(event) = rx.recv().await {
                seen.push(event);
            }
            seen
        });
        let (_stop, cancel) = watch::channel(false);
        let outcome = run_agent_turn(
            &backend,
            CompletionRequest {
                tools: Vec::new(),
                model: "scripted".into(),
                messages: vec![Message::user("what is in notes.txt?")],
                context_limit: None,
                temperature: None,
                seed: None,
                constraint: None,
            },
            &tools,
            &fixture.sandbox,
            Limits::default().with_max_steps(max_steps),
            schema_retry,
            tx,
            cancel,
        )
        .await;
        (drain.await.unwrap(), outcome)
    }

    /// A path that is open, readable and never answers: a FIFO with nobody at
    /// the other end. It is how a wedged network mount behaves, without needing
    /// one.
    #[cfg(unix)]
    fn fifo(at: &std::path::Path) {
        let name = std::ffi::CString::new(at.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path and a mode; `mkfifo` touches no
        // memory of ours and returns -1 rather than trapping.
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o644) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_that_never_answers_is_abandoned_rather_than_waited_on() {
        // The half that had nothing watching it. `Runtime::Host` runs tools in
        // this process, where there is no seam to hold a clock and no worker to
        // kill — and a `read_file` on something that never answers used to hang
        // the turn, the job and the session in silence.
        //
        // The test is also the proof that the blocking read had to move off the
        // poll thread: with `std::fs::read_to_string` inline in the future, the
        // task never yields, the timer is never polled, and this hangs forever
        // instead of failing.
        let fixture = Fixture::new("wedged");
        fifo(&fixture.root.join("wedged.fifo"));

        let backend = Scripted::new(&[
            "```tool\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"wedged.fifo\"}}\n```",
            "I could not read it.",
        ]);
        let tools = crate::tools::Tools::standard();
        let (tx, mut rx) = mpsc::channel(256);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let (_stop, cancel) = watch::channel(false);

        let started = std::time::Instant::now();
        let outcome = run_agent_turn(
            &backend,
            CompletionRequest {
                tools: Vec::new(),
                model: "scripted".into(),
                messages: vec![Message::user("read the fifo")],
                context_limit: None,
                temperature: None,
                seed: None,
                constraint: None,
            },
            &tools,
            &fixture.sandbox,
            Limits::default().with_tool_timeout(Duration::from_millis(300)),
            None,
            tx,
            cancel,
        )
        .await;
        let _ = drain.await;

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the turn has to end without the read ever answering",
        );
        let step = outcome.steps.first().expect("the call was made");
        let said = step.outcome.error.clone().unwrap_or_default();
        assert!(said.contains("did not answer in 300 ms"), "{said}");
        assert!(said.contains("abandoned"), "{said}");
        assert!(
            !step.outcome.verdict.allowed,
            "a call nobody answered is not an allowed one",
        );
        // And the turn carries on: the model gets the failure and answers.
        assert_eq!(outcome.text, "I could not read it.");

        // The abandoned read is still parked in the kernel, and dropping a
        // runtime waits for every blocking thread it started — so a test that
        // just ended here would hang on the way out. That is not an artefact of
        // testing: it is the same fact `bin/luu.rs` bounds with
        // `shutdown_timeout`, and unblocking the reader is how this test says
        // so out loud.
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .open(fixture.root.join("wedged.fifo"))
                .expect("a reader is parked on it, so opening to write returns"),
        );
    }

    #[tokio::test]
    async fn the_deadline_never_fires_before_the_tools_own_clock() {
        // The property that makes the clock safe to have on by default, and it
        // is the loop's version of the one the seam already held: a
        // `run_command` that asked for five minutes gets five minutes *plus*
        // the patience, and a tool with no clock of its own gets the patience.
        let limits = Limits::default().with_tool_timeout(Duration::from_millis(1_000));
        let read = crate::tools::ToolCall {
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "x"}),
        };
        let long = crate::tools::ToolCall {
            name: "run_command".into(),
            arguments: serde_json::json!({"argv": ["sleep"], "timeout_ms": 300_000}),
        };
        assert_eq!(limits.deadline(&read), Duration::from_millis(1_000));
        assert_eq!(limits.deadline(&long), Duration::from_millis(301_000));
    }

    #[tokio::test]
    async fn every_model_call_is_announced_with_what_it_sends() {
        // A tooled turn is two calls, and the counts the backend reports on
        // `Ended` are summed over both. Before this event only the first was
        // measurable, so on any turn with a tool our count and the backend's
        // were not counts of the same thing — 1 590 against 3 552 on the run
        // that found it. See `RECORD/2026-08-27.the-m4-pro-run.completed.md`.
        let fixture = Fixture::new("modelcalls");
        let (events, _) = drive(
            &fixture,
            &[
                "Let me look.\n```tool\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"notes.txt\"}}\n```",
                "It says the answer is 42.",
            ],
            DEFAULT_MAX_STEPS,
        )
        .await;

        let calls: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ModelCall { step, messages, .. } => Some((*step, messages)),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls.len(),
            2,
            "one call per round trip, the first included"
        );
        assert_eq!(calls[0].0, 1);
        assert_eq!(calls[1].0, 2);
        assert_eq!(
            calls[1].1.len(),
            calls[0].1.len() + 2,
            "the second call carries the assistant's call and the tool's result",
        );
    }

    #[tokio::test]
    async fn a_tool_call_is_executed_and_its_result_comes_back_to_the_model() {
        let fixture = Fixture::new("readloop");
        let (events, outcome) = drive(
            &fixture,
            &[
                "Let me look.\n```tool\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"notes.txt\"}}\n```",
                "It says the answer is 42.",
            ],
            DEFAULT_MAX_STEPS,
        )
        .await;

        assert_eq!(outcome.text, "It says the answer is 42.");
        assert_eq!(outcome.steps.len(), 1);
        assert!(outcome.steps[0].outcome.output.contains("42"));
        assert_eq!(outcome.reason, EndReason::Stop);

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TurnEvent::Ended { .. }))
                .count(),
            1,
            "a turn ends once, however many model calls it took",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TurnEvent::ToolCall { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TurnEvent::ToolResult { .. }))
        );
    }

    #[tokio::test]
    async fn a_denied_call_is_reported_to_the_model_rather_than_ending_the_turn() {
        let fixture = Fixture::new("denied");
        let (_, outcome) = drive(
            &fixture,
            &[
                "```tool\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"/etc/passwd\"}}\n```",
                "I cannot read that.",
            ],
            DEFAULT_MAX_STEPS,
        )
        .await;

        assert_eq!(outcome.steps.len(), 1);
        assert!(!outcome.steps[0].outcome.verdict.allowed);
        assert!(
            outcome.steps[0].result_text().contains("denied"),
            "the model is told, and can try something else"
        );
        assert_eq!(outcome.text, "I cannot read that.");
    }

    #[tokio::test]
    async fn a_plain_answer_makes_no_calls() {
        let fixture = Fixture::new("plain");
        let (events, outcome) = drive(&fixture, &["It is a text file."], DEFAULT_MAX_STEPS).await;
        assert!(outcome.steps.is_empty());
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, TurnEvent::ToolCall { .. }))
        );
    }

    fn schema_retry() -> SchemaRetry {
        SchemaRetry {
            schema: crate::tools::Tools::standard().call_schema(),
            tool_names: crate::tools::Tools::standard().names().collect(),
        }
    }

    #[tokio::test]
    async fn a_drifted_call_is_retried_once_under_the_schema_and_then_runs() {
        // The one retry `Constraint::Schema` is for: fenced under the tool's
        // own name instead of ```tool, the exact drift
        // RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md names throughout.
        // The scripted retry reply is what a schema-constrained server is
        // supposed to guarantee — the correct envelope — and the loop should
        // execute it rather than giving up on the first, malformed attempt.
        let fixture = Fixture::new("schema-retry-drift");
        let retry = schema_retry();
        let (events, outcome) = drive_with(
            &fixture,
            &[
                "```list_dir\n{}\n```",
                "```tool\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"notes.txt\"}}\n```",
                "It says the answer is 42.",
            ],
            DEFAULT_MAX_STEPS,
            Some(&retry),
        )
        .await;

        assert_eq!(outcome.text, "It says the answer is 42.");
        assert_eq!(outcome.steps.len(), 1, "the retried call is the one run");
        assert!(outcome.steps[0].outcome.output.contains("42"));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TurnEvent::ModelCall { .. }))
                .count(),
            3,
            "the drifted attempt, its schema-constrained retry, and the call after the tool result",
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TurnEvent::ToolCall { .. }))
                .count(),
            1,
            "one call executed, not the drifted attempt and the retry both",
        );
    }

    #[tokio::test]
    async fn a_clean_decline_is_never_retried() {
        // A schema cannot tell "chose not to call" from "forgot to" — only
        // score_call's Drifted can, and NoCall is not it. Retrying here would
        // force a call the model never attempted, which is exactly the "a
        // constrained model cannot say no" cost the WIP record names.
        let fixture = Fixture::new("schema-retry-noretry");
        let retry = schema_retry();
        let (events, outcome) = drive_with(
            &fixture,
            &["It is a text file.", "unreachable: never scripted to run"],
            DEFAULT_MAX_STEPS,
            Some(&retry),
        )
        .await;

        assert_eq!(outcome.text, "It is a text file.");
        assert!(outcome.steps.is_empty());
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TurnEvent::ModelCall { .. }))
                .count(),
            1,
            "a clean decline earns no retry",
        );
    }

    #[tokio::test]
    async fn a_refused_retry_falls_back_to_the_drifted_answer_instead_of_failing_the_turn() {
        // What a refused grammar (or schema) does to a session, named still
        // open throughout RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md:
        // the pre-retry attempt is a real reply the model produced, and
        // discarding it because the server would not compile the retry's
        // constraint would trade a working answer for nothing.
        let fixture = Fixture::new("schema-retry-refused");
        let retry = schema_retry();
        let backend = DriftThenRefuse::new("```list_dir\n{}\n```");
        let tools = crate::tools::Tools::standard();
        let (tx, mut rx) = mpsc::channel(256);
        let drain = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(event) = rx.recv().await {
                seen.push(event);
            }
            seen
        });
        let (_stop, cancel) = watch::channel(false);
        let outcome = run_agent_turn(
            &backend,
            CompletionRequest {
                tools: Vec::new(),
                model: "scripted".into(),
                messages: vec![Message::user("list the directory")],
                context_limit: None,
                temperature: None,
                seed: None,
                constraint: None,
            },
            &tools,
            &fixture.sandbox,
            Limits::default(),
            Some(&retry),
            tx,
            cancel,
        )
        .await;
        let events = drain.await.unwrap();

        assert!(
            outcome.error.is_none(),
            "a refused retry is not the turn failing"
        );
        assert_eq!(
            outcome.text, "```list_dir\n{}\n```",
            "the pre-retry drifted answer survives, not an empty or failed one"
        );
        assert!(outcome.steps.is_empty(), "no call was ever executed");
        assert!(
            !events.iter().any(|e| matches!(e, TurnEvent::Failed(_))),
            "the turn ends normally, not on a Failed event"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                TurnEvent::ConstraintRefused { error } if error.contains("failed to parse grammar")
            )),
            "the refusal is reported loudly, once, with the backend's own error"
        );
    }

    #[tokio::test]
    async fn a_model_that_never_answers_stops_at_the_limit_and_says_so() {
        let fixture = Fixture::new("limit");
        let looping = "```tool\n{\"name\":\"list_dir\",\"arguments\":{}}\n```";
        let (_, outcome) = drive(&fixture, &[looping, looping, looping, looping], 2).await;

        assert_eq!(outcome.steps.len(), 2);
        assert_eq!(
            outcome.reason,
            EndReason::ToolLimit,
            "`stop` would present an investigation cut short as a conclusion",
        );
    }

    #[tokio::test]
    async fn usage_is_summed_over_the_calls_the_turn_actually_made() {
        let fixture = Fixture::new("usage");
        let (_, outcome) = drive(
            &fixture,
            &[
                "```tool\n{\"name\":\"list_dir\",\"arguments\":{}}\n```",
                "There is one file.",
            ],
            DEFAULT_MAX_STEPS,
        )
        .await;
        // Two model calls at 10 prompt tokens each: the prefix was sent twice
        // and paid for twice, which is the number worth reporting.
        assert_eq!(outcome.usage.unwrap().prompt_tokens, 20);
    }

    #[tokio::test]
    async fn the_verdict_travels_with_the_result() {
        let fixture = Fixture::new("verdict");
        let (events, _) = drive(
            &fixture,
            &[
                "```tool\n{\"name\":\"list_dir\",\"arguments\":{}}\n```",
                "done",
            ],
            DEFAULT_MAX_STEPS,
        )
        .await;

        let result = events
            .iter()
            .find_map(|event| match event {
                TurnEvent::ToolResult { outcome, .. } => Some(outcome),
                _ => None,
            })
            .unwrap();
        assert!(result.outcome.verdict.allowed);
        assert_eq!(
            result.outcome.verdict.enforced_by,
            Applied::Process,
            "an in-process tool is held by an in-process check, and says so",
        );
        assert!(!result.outcome.verdict.rule.is_empty());
    }

    #[tokio::test]
    async fn the_steps_stay_in_the_conversation_the_model_sees() {
        // The point of storing them: on the second step the model is looking at
        // its own call and the result, not at a prompt where neither happened.
        let fixture = Fixture::new("history");
        let (_, outcome) = drive(
            &fixture,
            &[
                "```tool\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"notes.txt\"}}\n```",
                "42.",
            ],
            DEFAULT_MAX_STEPS,
        )
        .await;
        assert!(outcome.steps[0].text.contains("read_file"));
        assert!(outcome.steps[0].result_text().starts_with("[read_file] ok"));
    }
}
