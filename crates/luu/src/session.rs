//! What `chat` and `serve` must not each invent.
//!
//! Both drive the same loop, and both need the same prompt assembled the same
//! way. Two call sites building it by hand is exactly what the byte-identical
//! prefix commitment in `AGENTS.md` warns about, so there is one.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::backend::Message;
use agent_core::context::{ApproximateCounter, Budget, Counter, ModelCounter, TokenCounter};
use agent_core::protocol::{self, ServerMessage, TurnId};
use agent_core::record::{self, RecordLine};
use agent_core::sandbox::{NULL_DEVICE, Sandbox};
use agent_core::tools::Tools;
use agent_core::trace::TraceMessage;
use agent_core::worker::{Executor, Worker};
use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// The fixed head of every prompt. Kept as one `&'static str` because the
/// prompt cache reuses it byte for byte — a formatting change here is a cache
/// miss on every call, and nothing fails to tell you.
pub const SYSTEM: &str = "You are luu, a concise local coding agent.";

/// What the agent is asked before a task starts, fused into the current user
/// message and never into the system block.
///
/// That placement is the whole reason this is a `&str` here rather than a line
/// appended to [`SYSTEM`]: the prefix is what the cache reuses, and a planning
/// instruction living up there would be paid for on every call of every turn,
/// including the ones that never plan.
pub const PLANNING: &str = "\
Before anything runs, propose a plan for the work below. Reply with one fenced \
block and nothing after it:

```plan
{\"objective\": \"what this piece of work is\", \"tasks\": [\"what you will do\"], \
\"files\": [\"paths you will read\"], \"writes\": [\"paths you will change\"], \
\"commands\": [\"programs you will run\"]}
```

A path you will change goes in `writes`, and you may not change one that is \
only in `files`. Name only what this piece of work needs; leave a list empty if \
it needs none. \
The person will read this and approve it or refuse it before you run anything.

The work:
";

/// What the planning call is asked *over*, now that it is not asked over a held
/// prompt.
///
/// [`PLANNING`] ends on "The work:" and something has to follow it. Before item
/// 22 that was the prompt a person had just typed, unexplored; under the
/// alternation it is the draft — **the plan is what the draft compacts to** —
/// so this names the window rather than restating a question, and says which
/// half of the draft to keep. See
/// `RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md` §third.
///
/// Kept beside [`PLANNING`] and not inlined at the call site for that
/// constant's own reason: what the model is asked is a thing this project
/// changes deliberately and measures, and a prompt written inline in a handler
/// is a prompt nobody diffs.
pub const PLAN_OVER_DRAFT: &str = "The conversation above is the draft: what was asked, what was read, and what was worked out. Plan the work it arrived at. Keep what was decided and leave out what was tried and dropped — the plan replaces the draft, so anything it does not name is not carried forward.";

/// The sandbox and the tool set, resolved once and shared by `chat` and
/// `serve`. Here for the same reason the prompt assembly is: two call sites
/// resolving a policy differently is two sandboxes.
#[derive(Clone)]
pub struct Agency {
    pub tools: Arc<Tools>,
    pub sandbox: Arc<Sandbox>,
    /// The same policy with every grant downgraded to a read — what holds a
    /// turn that no approved plan holds.
    ///
    /// **Private, which is the whole of how it is kept honest.** It is derived
    /// from `sandbox` in [`Agency::new`] and nowhere else, so a second call
    /// site cannot resolve it differently and a seventh cannot forget it: with
    /// one private field a struct literal no longer compiles outside this
    /// module, and `new` is the only door. That is this struct's own doc
    /// comment — *two call sites resolving a policy differently is two
    /// sandboxes* — enforced by the type rather than asserted in prose, after
    /// **six** literals had accumulated under it. Two of the six were found by
    /// making the field private and not by the search that preceded it, which
    /// is the argument for the type doing the remembering.
    ///
    /// **Read by `serve` and not by `chat`**, because a floor is the gate's
    /// shadow and `chat` has no gate — the policy file is its standing approval
    /// and the operator who ran the command is who that was. See
    /// `RECORD/2026-09-21.the-drafts-floor.completed.md`.
    floor: Arc<Sandbox>,
    /// What bounds a turn: how many tool calls, and how long any one of them
    /// may take. The timeout is the *loop's*, so it holds under
    /// `runtime = "host"` as much as behind a container — which is the whole
    /// point of it living up here. See
    /// `RECORD/2026-09-05.a-clock-where-there-is-no-seam.completed.md`.
    pub limits: agent_core::agent::Limits,
    /// Where a tool call actually runs, when that is not this process.
    ///
    /// `None` is `runtime = "host"`, which is every run this repository has
    /// made so far and stays the default. `Some` is a `luu worker` on the other
    /// end of a pipe — a plain child under `direct`, and the container's only
    /// process under a container runtime. See
    /// `RECORD/2026-09-02.the-worker-and-the-seam.completed.md`.
    ///
    /// The *definitions* never move: they are the second half of the cached
    /// prefix, and a prefix assembled inside the image is one that shifts every
    /// time the image is rebuilt.
    pub worker: Option<Arc<Worker>>,
}

impl Agency {
    /// The only way to build one, so the floor is derived exactly once.
    ///
    /// See [`Agency::floor`] for why that matters and why the field is private.
    pub fn new(
        tools: Arc<Tools>,
        sandbox: Arc<Sandbox>,
        limits: agent_core::agent::Limits,
        worker: Option<Arc<Worker>>,
    ) -> Self {
        Self {
            tools,
            floor: Arc::new(sandbox.read_only()),
            sandbox,
            limits,
            worker,
        }
    }

    /// What holds a turn no approved plan holds. See [`Agency::floor`].
    pub fn floor(&self) -> &Arc<Sandbox> {
        &self.floor
    }

    /// This posture, as a recording names it — the three facts a reader compares
    /// two runs on, and the name of the file they came from when a session
    /// chose one. See `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
    pub fn posture(&self, name: Option<String>) -> record::Posture {
        record::Posture {
            name,
            runtime: match &self.worker {
                Some(worker) => worker.label().to_string(),
                None => "host".to_string(),
            },
            enforcement: self.sandbox.enforcement().as_str().to_string(),
            network: self.sandbox.network(),
        }
    }

    /// Ends the worker this posture started, if it started one.
    ///
    /// Explicit rather than left to `kill_on_drop`, and for the reason
    /// `Pipe::kill` gives: "eventually" is not a property worth having where the
    /// thing being ended is a container. A session that has been replaced must
    /// not leave one behind, which is the claim this whole seam is built on.
    pub async fn shutdown(&self) {
        if let Some(worker) = &self.worker {
            worker.end().await;
        }
    }

    /// The tool definitions as they go into the cached prefix. Empty when there
    /// are no tools, so a run without them sends the same bytes it always did.
    pub fn definitions(&self) -> String {
        self.tools.definitions()
    }

    /// The context with this session's tools in it, under the transport its
    /// destination uses: as text in the system message (`fenced`), or counted
    /// and sent in the request's own `tools` field (`native`). See
    /// `RECORD/2026-09-30.native-tool-calls.completed.md`.
    pub fn with_tools(
        &self,
        context: agent_core::context::Context,
        tool_calls: crate::provider::ToolCalls,
    ) -> agent_core::context::Context {
        match tool_calls {
            crate::provider::ToolCalls::Fenced => context.with_tools(self.definitions()),
            crate::provider::ToolCalls::Native => {
                context.with_native_tools(self.tools.native_definitions())
            }
        }
    }

    /// What a request carries in its `tools` field: every tool under
    /// `native`, nothing under `fenced`.
    pub fn specs(
        &self,
        tool_calls: crate::provider::ToolCalls,
    ) -> Vec<agent_core::backend::ToolSpec> {
        match tool_calls {
            crate::provider::ToolCalls::Fenced => Vec::new(),
            crate::provider::ToolCalls::Native => self.tools.specs(),
        }
    }

    /// Where tool calls go. The seam, resolved once: the loop asks this and
    /// never asks where it runs.
    pub fn executor(&self) -> &dyn Executor {
        match &self.worker {
            Some(worker) => worker.as_ref(),
            None => self.tools.as_ref(),
        }
    }

    /// What `luu tools` prints and what a session says on startup.
    ///
    /// Every grant is listed, the implicit ones included: a sandbox whose real
    /// extent has to be inferred from the file that configured it is one nobody
    /// checks.
    pub fn describe(&self) -> String {
        let mut text = format!("sandbox — base {}\n", self.sandbox.base().display());
        for root in self.sandbox.roots() {
            text.push_str(&format!(
                "  {:<10} {}{}\n",
                root.access.as_str(),
                root.path.display(),
                // Two implicit grants with two reasons, and one line each: the
                // system trees are there so a command can read its interpreter
                // and libc, `/dev/null` so it can open the fds every program
                // assumes — `git` could not start without it. See
                // `RECORD/2026-09-15.git-could-not-open-dev-null.completed.md`.
                match (root.implicit, root.path.as_path() == Path::new(NULL_DEVICE)) {
                    (true, true) => "   (implicit: programs open the null device)",
                    (true, false) => "   (implicit: commands need their interpreter)",
                    (false, _) => "",
                }
            ));
        }
        text.push_str(&format!(
            "  commands   {}\n  network    {}\n  enforce    {}\n",
            match self.sandbox.commands().is_empty() {
                true => "(none)".to_string(),
                false => self.sandbox.commands().join(", "),
            },
            match self.sandbox.network() {
                true => "allowed",
                false => "denied",
            },
            self.sandbox.enforcement().as_str(),
        ));
        // Beside `enforce`, because it is the same question: what holds a
        // child. On macOS it is the only line here that is true.
        text.push_str(&format!(
            "  limits     {}\n",
            self.sandbox
                .limits()
                .describe()
                .unwrap_or_else(|| "(none: the clock alone)".to_string()),
        ));
        // The loop's clock, printed whether or not there is a worker: under
        // `runtime = "host"` there is nothing else that would mention it, and
        // that is exactly the mode where it used to be missing.
        text.push_str(&format!(
            "  deadline   {} ms per tool call, plus whatever the call asked for\n",
            self.limits.tool_timeout.as_millis(),
        ));
        if let Some(worker) = self.worker.as_ref().and_then(|worker| worker.describe()) {
            text.push_str(&worker);
        }
        text.push_str(&format!(
            "  tools      {}\n",
            match self.tools.is_empty() {
                true => "(none)".to_string(),
                false => self.tools.names().collect::<Vec<_>>().join(", "),
            }
        ));
        text
    }
}

/// How much of the window is held back for the answer. A default, not a law:
/// too large wastes context, too small truncates answers, and the only way to
/// find out is to measure.
pub const DEFAULT_RESERVE: u32 = 512;

/// Builds the counter, and says out loud when it is not a real one.
///
/// An explicit `--tokenizer` that fails is an error: it was asked for. An
/// absent one degrades to the approximate counter with a warning, so a first
/// run works without hunting down a `tokenizer.json` — and every number it
/// produces is labelled all the way to the panel.
pub fn counter_for(
    model: &str,
    tokenizer: Option<&Path>,
) -> Result<(Arc<dyn TokenCounter>, Option<String>)> {
    match tokenizer {
        Some(path) => {
            let counter = ModelCounter::from_file(path, model)
                .with_context(|| format!("--tokenizer {}", path.display()))?;
            Ok((Arc::new(counter), None))
        }
        None => Ok((
            Arc::new(ApproximateCounter),
            Some(format!(
                "no tokenizer for {model}: counting approximately (chars/4). \
                 Pass --tokenizer <tokenizer.json> for real numbers."
            )),
        )),
    }
}

/// What the backend is about to receive, as one string, for the trace channel.
/// Not the wire format any backend uses — it is a rendering, and the panel that
/// diffs two of them only needs them to be rendered the same way twice.
pub fn rendered(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| format!("<|{:?}|>\n{}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The previous turn's rendered prompt, kept so the next one can be measured
/// against it.
///
/// Here rather than in `agent-core` because the thing measured is a rendering,
/// and the rendering is this module's; here rather than in each caller because
/// `chat` and `serve` measuring against differently-assembled prompts is the
/// same failure the module exists to prevent.
#[derive(Default)]
pub struct PrefixTracker {
    previous: Option<String>,
}

impl PrefixTracker {
    /// Measures this turn's prompt against the one before it, and remembers it
    /// for the next. `None` on the first turn of a session: there is no
    /// previous prompt, so the quantity does not exist — and "0% reuse" would
    /// read as a cold cache rather than as the absence of one.
    pub fn measure(
        &mut self,
        turn: TurnId,
        prompt: &str,
        counter: &dyn TokenCounter,
    ) -> Option<TraceMessage> {
        let message = self
            .previous
            .as_deref()
            .map(|previous| TraceMessage::prefix_reuse(turn, previous, prompt, counter));
        self.previous = Some(prompt.to_string());
        message
    }
}

/// One message on its way to clients, to the record, or both.
#[derive(Clone)]
pub enum Event {
    Protocol(ServerMessage),
    Trace(TraceMessage),
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The first line of every stream, wherever the stream is kept.
///
/// One function because there are now two places that start one — the recorder
/// writing a `.jsonl` and the store keeping a session's own — and a header
/// assembled twice is two claims about what a run was comparable with. See
/// `RECORD/2026-09-04.the-border-and-the-gate.completed.md`.
#[allow(clippy::too_many_arguments)]
pub fn header(
    backend: &str,
    model: &str,
    budget: Budget,
    counter: Counter,
    posture: Option<record::Posture>,
    // What this session's config says a draft and a plan are told, in the
    // wire's shape already — `luu chat` has no `Authority::Draft`, so its
    // only caller passes `AuthorityNotes::default()` and both fields go out
    // `None`, the same as a stream from before format 18. See
    // `RECORD/2026-09-22.an-authority-a-model-is-told.completed.md`.
    authority: &crate::provider::AuthorityNotes,
    // Written only when native — see `record::FORMAT` 19.
    tool_calls: crate::provider::ToolCalls,
    started_at: u64,
) -> RecordLine {
    RecordLine::Header {
        format: record::FORMAT,
        protocol: protocol::VERSION,
        backend: backend.to_string(),
        model: model.to_string(),
        context_limit: budget.limit,
        counter: Some(counter),
        eviction: Some(budget.eviction),
        posture,
        // The other three the budget is holding. This function took a whole
        // `Budget` and wrote down two of its five fields until format 11, so a
        // run under `--repeat-once` left nothing behind saying so — while the
        // doc comments on the builders that set them gave the reason it should
        // have. See `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`.
        repeat: Some(budget.repeat),
        prune: Some(budget.prune),
        results: Some(budget.results),
        authority_draft: authority.draft_note(),
        authority_plan: authority.plan_note(),
        tool_calls: tool_calls.native().then(|| tool_calls.as_str().to_string()),
        started_at,
    }
}

/// One event as the line that records it. Beside [`header`] and for the same
/// reason: the file and the store keep the same stream, so they had better turn
/// an event into a line the same way.
pub fn line(event: &Event, at_ms: u64) -> RecordLine {
    match event {
        Event::Protocol(message) => RecordLine::Protocol {
            at_ms,
            message: message.clone(),
        },
        Event::Trace(message) => RecordLine::Trace {
            at_ms,
            message: message.clone(),
        },
    }
}

/// Writes the JSON-lines record, on its own task so a slow disk never stalls a
/// turn.
pub struct Recorder {
    lines: mpsc::UnboundedSender<RecordLine>,
    /// What every line's `at_ms` is counted from — **the header above it**, not
    /// the process.
    ///
    /// Atomic because it moves: a `serve` that starts or resumes a session
    /// writes a second header through [`Self::session`], and a header declares
    /// the base the lines after it are relative to. It was a plain `u64` while
    /// a file could only ever hold one session's worth of header, which is the
    /// state item 23 describes. See
    /// `RECORD/2026-09-19.a-header-per-session.completed.md`.
    started_at: std::sync::atomic::AtomicU64,
}

impl Recorder {
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        path: &Path,
        backend: &str,
        model: &str,
        budget: Budget,
        counter: Counter,
        // What the run was allowed to do, so a `--record` file can be read
        // back against another one. See
        // `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
        posture: Option<record::Posture>,
        authority: &crate::provider::AuthorityNotes,
        tool_calls: crate::provider::ToolCalls,
        started_at: u64,
    ) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let mut file = tokio::fs::File::create(path)
            .await
            .with_context(|| format!("creating {}", path.display()))?;

        let header = header(
            backend, model, budget, counter, posture, authority, tool_calls, started_at,
        );
        file.write_all(format!("{}\n", serde_json::to_string(&header)?).as_bytes())
            .await?;

        let (lines, mut rx) = mpsc::unbounded_channel::<RecordLine>();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let Ok(json) = serde_json::to_string(&line) else {
                    continue;
                };
                if file
                    .write_all(format!("{json}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                // Flushed per line: a session worth replaying is usually one
                // that ended badly, and a buffered tail is the part you needed.
                let _ = file.flush().await;
            }
        });

        Ok(Self {
            lines,
            started_at: std::sync::atomic::AtomicU64::new(started_at),
        })
    }

    /// Starts recording a different session: writes its header and re-bases.
    ///
    /// The two are one call because they are one decision. A header without the
    /// re-base leaves every line after it counted from a moment the file no
    /// longer names, and a re-base without a header shifts every timestamp with
    /// nothing saying why — so neither half is reachable on its own.
    ///
    /// `started_at` is the **session's** and not now: a resumed session started
    /// when it started, so the lines after its header carry the offset from
    /// that, which can be hours. Correct, and the same number the store's
    /// stream for that session already carries.
    #[allow(clippy::too_many_arguments)]
    pub fn session(
        &self,
        backend: &str,
        model: &str,
        budget: Budget,
        counter: Counter,
        posture: Option<record::Posture>,
        authority: &crate::provider::AuthorityNotes,
        tool_calls: crate::provider::ToolCalls,
        started_at: u64,
    ) {
        // Stored before the line is sent, so a turn published between the two
        // is counted from the header it will be read under rather than from the
        // one it is replacing.
        self.started_at
            .store(started_at, std::sync::atomic::Ordering::Relaxed);
        let _ = self.lines.send(header(
            backend, model, budget, counter, posture, authority, tool_calls, started_at,
        ));
    }

    pub fn write(&self, event: &Event) {
        let at_ms =
            now_ms().saturating_sub(self.started_at.load(std::sync::atomic::Ordering::Relaxed));
        let _ = self.lines.send(line(event, at_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::context::{Eviction, Prune, Repeat, Results};

    /// The defect format 11 was taken for: this function is handed a whole
    /// `Budget` and wrote down two of its five fields.
    ///
    /// It is pinned here rather than on the format, because the format only
    /// makes the fields *possible*. Both places that start a stream come
    /// through here — the recorder writing a `.jsonl` and the store keeping a
    /// session's own — so a header that drops a field drops it everywhere, and
    /// silently: every arm still reads back as a valid header. See
    /// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`.
    #[test]
    fn a_header_writes_down_every_rule_the_budget_is_holding() {
        let budget = Budget::new(8192, 512, Eviction::Turn)
            .repeating(Repeat::Once)
            .pruning(Prune::Behind)
            .citing(Results::Cited);

        match header(
            "mock",
            "mock",
            budget,
            Counter::Approximate,
            None,
            &crate::provider::AuthorityNotes::default(),
            crate::provider::ToolCalls::Fenced,
            0,
        ) {
            RecordLine::Header {
                repeat,
                prune,
                results,
                ..
            } => assert_eq!(
                (repeat, prune, results),
                (
                    Some(Repeat::Once),
                    Some(Prune::Behind),
                    Some(Results::Cited)
                ),
                "the arm a run was is in this function's hand; it has to reach the stream",
            ),
            other => panic!("{other:?}"),
        }

        // And the other arm, because a function that hard-coded one value would
        // pass the assertion above. It is `Always` that has to be asked for by
        // name since 2026-09-19 — the flip moved which of the two is the
        // silent one, and this test is only interested in there being two.
        match header(
            "mock",
            "mock",
            Budget::new(8192, 512, Eviction::Turn).repeating(Repeat::Always),
            Counter::Approximate,
            None,
            &crate::provider::AuthorityNotes::default(),
            crate::provider::ToolCalls::Fenced,
            0,
        ) {
            RecordLine::Header {
                repeat,
                prune,
                results,
                ..
            } => assert_eq!(
                (repeat, prune, results),
                (
                    Some(Repeat::Always),
                    Some(Prune::Never),
                    Some(Results::Kept)
                ),
                "a run says which arm it was, whichever arm that is; only a file \
                 older than the format stays silent",
            ),
            other => panic!("{other:?}"),
        }
    }
}
