//! `luu serve` — the local HTTP server behind the debug UI.
//!
//! Loopback by default and unauthenticated: it exposes an agent that runs
//! commands. Binding it anywhere else requires a bearer token, and [`bind`]
//! refuses to hold the port without one — see [`crate::auth`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::agent::{SchemaRetry, run_agent_turn};
use agent_core::api::SessionView;
use agent_core::approval::{Approval, Approvers, Signature};
use agent_core::backend::{Backend, CompletionRequest, Constraint};
use agent_core::context::{Budget, Context as AgentContext, Fragment, TokenCounter};
use agent_core::job::{ClosedBy, JobId, Plan, PlanSource, Proposal, parse_plan};
use agent_core::protocol::{self, ClientMessage, Refusal, ServerMessage, TurnId};
use agent_core::record;
use agent_core::repo_map::{Order, RepoMap};
use agent_core::sandbox::{Authority, Sandbox};
use agent_core::trace::TraceMessage;
use agent_core::turn::{EndReason, TurnEvent, run_turn};

use crate::auth::Auth;
use crate::session::{
    Agency, Event, PLAN_OVER_DRAFT, PLANNING, PrefixTracker, Recorder, SYSTEM, now_ms, rendered,
};
use crate::store::SessionStore;
use crate::workspace;
use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, watch};

/// The UI, embedded in the binary.
///
/// `rust-embed` reads these from disk in debug builds and bakes them in for
/// release, which is exactly the split we want: editing a component must not
/// cost a `cargo build`, and a shipped binary must not need the files.
/// The folder is `web/` at the root of the repository, not inside this crate:
/// the page is a surface of its own, which a server other than the one on this
/// machine may come to serve. `node_modules` is excluded because Monaco is a
/// node dependency of `web/` —
/// gitignored like every other one, and served from disk by `monaco_asset`
/// rather than baked in. Without this line a release binary would carry several
/// megabytes of an editor that is off by default.
#[derive(rust_embed::Embed)]
#[folder = "../../web/"]
#[exclude = "node_modules/*"]
struct Ui;

struct Session {
    next_turn: TurnId,
    current: Option<TurnId>,
    /// Present only while a turn is running.
    cancel: Option<watch::Sender<bool>>,
    /// The conversation so far. In memory: enough to measure a context
    /// strategy, and lost on restart until sessions are persisted.
    context: AgentContext,
    /// Beside the context, because what it measures is a property of two
    /// consecutive prompts of *this* session.
    prefix: PrefixTracker,
    /// A proposal waiting on a person, holding the prompt that caused it.
    /// While this is set, nothing runs: not a turn, not a tool, not a model
    /// call. That is the gate.
    pending: Option<Pending>,
    /// The live job's own sandbox: the approved plan, resolved against the
    /// policy file. Every turn inside the job is checked against this rather
    /// than against the session's, which is what makes the job boundary the
    /// scope permission is granted at instead of a comment saying it is.
    /// `None` outside a job, where the policy file is the whole answer.
    narrowed: Option<(JobId, Arc<Sandbox>)>,
    /// A call the floor refused and a person is being asked about — at most
    /// one, since a turn makes one call at a time and one turn runs. The turn
    /// is waiting on `answer`; nothing else is. See
    /// `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
    held: Option<Held>,
}

/// One call waiting on *allow once* or *deny*.
struct Held {
    turn: TurnId,
    step: u32,
    answer: tokio::sync::oneshot::Sender<bool>,
}

/// The person a drafting turn asks, by way of whoever is watching the session.
///
/// Publishes [`ServerMessage::CallHeld`] and waits for
/// [`ClientMessage::AnswerCall`]. A sender dropped without an answer — the
/// session replaced under it — is a *no*, the answer that leaves things as the
/// floor left them.
struct Watching {
    app: Arc<App>,
    turn: TurnId,
    /// The turn's own event channel. The question goes out on it rather than
    /// straight to `publish`, because the `tool_call` it is about went out on
    /// it too, and a page that heard the question first had no call to hang it
    /// on.
    events: mpsc::Sender<TurnEvent>,
}

impl agent_core::agent::Asker for Watching {
    fn ask<'a>(
        &'a self,
        step: u32,
        call: &'a agent_core::tools::ToolCall,
        refused: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let (answer, answered) = tokio::sync::oneshot::channel();
            // Held before it is announced, so an answer can never arrive
            // for a call the session does not know is waiting.
            {
                let mut session = self.app.session.lock().await;
                session.held = Some(Held {
                    turn: self.turn,
                    step,
                    answer,
                });
            }
            let _ = self
                .events
                .send(TurnEvent::CallHeld {
                    step,
                    call: call.clone(),
                    refused: refused.to_string(),
                })
                .await;
            let allow = answered.await.unwrap_or(false);
            // Cleared here as well as by the answer, so a cancel that won the
            // race leaves nothing for a late answer to land on.
            let mut session = self.app.session.lock().await;
            if session
                .held
                .as_ref()
                .is_some_and(|held| held.turn == self.turn && held.step == step)
            {
                session.held = None;
            }
            allow
        })
    }
}

/// A prompt held between the proposal and the answer to it.
///
/// `prompt` is `None` for a proposal that outlived the process which made it:
/// resuming re-establishes the gate, and the prompt that bought the planning
/// call is long gone. Inventing one to give the gate something to release would
/// put words in a person's mouth, so approving such a job opens it and starts
/// nothing — the next prompt is a turn inside it.
/// The one plan on the table, while a person decides.
///
/// It holds **no job id and no held prompt**, and both absences are the shape
/// rather than a simplification. A proposal is offered inside the open draft
/// and an id is what approval hands out, so there is nothing to name until it
/// is answered; and no prompt is held any more, because a prompt runs in the
/// draft the moment it arrives. See
/// `RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md` §third.
struct Pending {
    objective: String,
    plan: Plan,
    source: PlanSource,
}

/// The id the live session is served under. There is one until sessions are
/// persisted, and naming it beats a magic string at four call sites.
pub const LIVE_SESSION: &str = "live";

/// Where the live session sends, and everything decided by that.
///
/// One struct because these are decided *together* and must move together. A
/// backend swapped without its budget budgets the new destination against the
/// old one's window; swapped without its counter, it counts one model's tokens
/// with another's tokenizer — and the session header, whose whole job is to say
/// what two runs may be compared as, would name the new backend beside the old
/// numbers.
///
/// Held behind an `RwLock` and read as a clone: a guard carried across an
/// `await` is a deadlock nothing in the tests would reliably find. It is
/// replaced **only** when a session is created — see `create_session`, and
/// `RECORD/2026-09-07.the-first-run-has-no-provider.completed.md` for why never
/// inside one.
struct Destination {
    backend: Arc<dyn Backend>,
    model: String,
    budget: Budget,
    counter: Arc<dyn TokenCounter>,
    /// The same resolution, rendered for the page.
    settings: Settings,
    /// What a draft or a plan is told about itself, this session's own choice
    /// of it. See `crate::provider::AuthorityNotes`.
    authority: crate::provider::AuthorityNotes,
    /// The engine luu starts for this destination, when it names one: warmed
    /// once the session is pointed here, and waited on by `backend` itself.
    engine: Option<(String, crate::provider::Engine)>,
    /// How tool calls travel to it. Decides how a session's context renders
    /// its tools and what each request carries — see
    /// `RECORD/2026-09-30.native-tool-calls.completed.md`.
    tool_calls: crate::provider::ToolCalls,
}

impl Destination {
    /// The same destination, rendering its window under different rules.
    ///
    /// Everything that decides *where* a turn goes is kept — the backend, the
    /// model, the counter — because none of it moved: what changed is how much
    /// of the history is resent to it. The settings go with the budget, so the
    /// page reads back the session it is watching rather than the one the
    /// process was started as.
    fn resending(&self, budget: Budget) -> Self {
        Self {
            backend: self.backend.clone(),
            model: self.model.clone(),
            budget,
            counter: self.counter.clone(),
            settings: Settings {
                repeat: budget.repeat,
                prune: budget.prune,
                results: budget.results,
                ..self.settings.clone()
            },
            authority: self.authority.clone(),
            engine: self.engine.clone(),
            tool_calls: self.tool_calls,
        }
    }

    /// The same destination with the window its server said it serves: the
    /// budget has a limit from here on, the page is told where it came from,
    /// and the caveat that the window is only the server's drops, because the
    /// server just said.
    fn windowed(&self, window: u32) -> Self {
        let budget = Budget {
            limit: Some(window),
            ..self.budget
        };
        Self {
            backend: self.backend.clone(),
            model: self.model.clone(),
            budget,
            counter: self.counter.clone(),
            settings: Settings {
                window: Some(window),
                window_from: crate::provider::WindowFrom::Server,
                window_caveat: None,
                ..self.settings.clone()
            },
            authority: self.authority.clone(),
            engine: self.engine.clone(),
            tool_calls: self.tool_calls,
        }
    }

    /// [`Self::resending`]'s mirror for the two notes instead of the three
    /// rules: everything that decides where a turn goes is kept, and what
    /// changed is what a draft or a plan is told about itself.
    fn noting(&self, authority: crate::provider::AuthorityNotes) -> Self {
        Self {
            backend: self.backend.clone(),
            model: self.model.clone(),
            budget: self.budget,
            counter: self.counter.clone(),
            settings: Settings {
                authority: authority.clone(),
                ..self.settings.clone()
            },
            authority,
            engine: self.engine.clone(),
            tool_calls: self.tool_calls,
        }
    }
}

struct App {
    /// The live session's destination. See [`Destination`].
    destination: RwLock<Arc<Destination>>,
    /// The tokenizer flag, kept because a destination built later needs the
    /// same answer this one got: a counter is per *model*, so a switch that
    /// left the old one in place would count the new model with it.
    tokenizer: Option<PathBuf>,
    session: Mutex<Session>,
    events: broadcast::Sender<Event>,
    recorder: Option<Recorder>,
    /// What this session is allowed to do, and where its tools run. Behind a
    /// lock for the same reason `destination` is: a session chooses it when it
    /// starts, and the choice is the session's rather than the process's. See
    /// `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
    agency: RwLock<Arc<Agency>>,
    /// The posture the live session named, or `None` for the server's own
    /// policy file.
    posture: Mutex<Option<String>>,
    /// How a session's posture is built: given a policy file, an agency.
    ///
    /// A function rather than the flags themselves, so this file knows nothing
    /// about the command line and a test can hand it one. `None` where sessions
    /// cannot choose a posture — over stdio, where the process is the session.
    agency_for: Option<AgencyFactory>,
    /// How to say where a posture would run without building it. `None`
    /// where `agency_for` is.
    describe_for: Option<DescribeFactory>,
    /// The postures this machine names: read when the server started, and
    /// replaced when Settings → Runtimes writes `config.toml`, so a posture
    /// added there is in the picker at once rather than after a restart. A
    /// session already on a posture keeps the agency it was built with. See
    /// `RECORD/2026-10-01.runtimes-in-settings.completed.md`.
    postures: std::sync::RwLock<std::collections::BTreeMap<String, crate::provider::Posture>>,
    /// Where those came from, for the page to name.
    postures_path: Option<String>,
    /// The icon theme `[ui] icon-theme` names, or an empty one when nothing
    /// names it or it did not load. Loaded at startup and swapped whole when
    /// Settings writes a new one — see [`put_icon_theme`]. The page follows
    /// through the manifest's `revision`, which is in every icon's URL.
    icons: std::sync::RwLock<Arc<crate::icons::Theme>>,
    /// The model servers this server started, and the downloads of luu's own
    /// copies. Children of this process — see [`Serving::run`] for how they
    /// stop with it, and `crate::engines` for the rest.
    engines: Arc<crate::engines::Supervisor>,
    /// What Settings → Runtimes started: installs, starts and image builds.
    /// See `crate::runtimes`.
    tasks: Arc<crate::runtimes::Tasks>,
    /// The requests being answered right now, for the log's watchdog. See
    /// `crate::log`.
    inflight: Arc<crate::log::InFlight>,
    /// Pinned sampling, forwarded to every call the same way `budget` is.
    /// `None` leaves it to the server's own default.
    temperature: Option<f32>,
    seed: Option<u32>,
    /// Sent on every step, unless `schema_retry` is instead spent once on a
    /// drifted reply. Built once at session start, the same reason `budget`
    /// is: a constraint compiled per turn would cost the compile every turn
    /// for a value that is the same every time. `None` is unconstrained, the
    /// only case every recording made before `--constrain` reached `serve`
    /// and `stdio` was measured under. See
    /// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
    constraint: Option<Constraint>,
    /// The retry `Constraint::Schema` is honest for, spent once per turn on a
    /// reply that drifted rather than sent on every attempt. `None` when
    /// `--constrain schema` was not asked for.
    schema_retry: Option<SchemaRetry>,
    /// The read side, folded from the same events the sockets carry — so
    /// `GET /api/...` can never disagree with what a client watched happen.
    view: Mutex<SessionView>,
    /// When the session now live started.
    ///
    ///
    /// Every `at_ms` is relative to this, so a session's lines are timed from
    /// its own beginning whether it was created here or resumed out of the
    /// store — and not from the process's clock, which stopped being the same
    /// number the moment `serve` learned to switch sessions: measuring from the
    /// process would give a resumed session a first turn several days in.
    session_started_at: Mutex<u64>,
    /// What this session is called on disk. The live view is served as
    /// [`LIVE_SESSION`] whatever it is called, because a client watching *the*
    /// session should not have to learn its name first — but a store keyed on
    /// "live" would overwrite the previous session on every restart, which is a
    /// history that only ever holds one thing.
    session_id: Mutex<String>,
    map_rendered: String,
    /// The tree a selection is chosen from, tagged at startup and **re-stamped
    /// before every turn** — `rewalk_sources` re-parses only the files whose
    /// mtime or length moved, because it is the parse rather than the choosing
    /// that is expensive. Empty when selection is off.
    walked: Mutex<Vec<agent_core::repo_map::Walked>>,
    /// Tokens of selected fragments per turn. 0 is off.
    select_tokens: u32,
    /// Which signals score a file.
    select_weights: agent_core::select::Weights,
    /// Where the fold is cached between restarts. `None` leaves the session in
    /// memory, which is what every run did before this existed.
    store: Option<Mutex<SessionStore>>,
    /// The lines this session has produced and the store has not been given
    /// yet, oldest first.
    ///
    /// The store keeps the stream as well as the fold — the fold is a cache and
    /// this is the thing it is a cache of — but a write per token would be
    /// quadratic in the length of a session for no reader's benefit. So the
    /// lines queue here and go down at the same checkpoints the fold does, in
    /// the same order they were published.
    stream: Mutex<Vec<record::RecordLine>>,
    /// Who may approve, and whether anyone must sign to. Empty and not
    /// required is the local operator with a keyboard, which is every session
    /// before `RECORD/2026-09-04.signed-approvals.completed.md`.
    approvers: Approvers,
}

/// Given the policy file a session named — or `None` for the server's own — the
/// agency that is it: the sandbox, the tools, the limits and the worker.
///
/// Boxed because it outlives the call that made it and is shared by every
/// session, and a function because `serve` should not know what a command line
/// is. See `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
/// Where a posture would run, as the environment picker shows it. See
/// `SandboxArgs::describe_posture`, which is the only thing that makes one.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Where {
    /// The policy file, as this machine spells it.
    pub policy: String,
    /// `host`, `direct`, or a container runtime.
    pub runtime: String,
    /// The image, for a runtime that takes one.
    pub image: Option<String>,
    /// Why a session cannot move there: the runtime is not installed, the
    /// file names no image, or the file does not load. `None` is "as far as
    /// can be told without starting it".
    pub missing: Option<String>,
    /// What this runtime cannot express, where it runs anyway — Apple's
    /// `container` and the network. See `Runtime::cannot`.
    pub gap: Option<String>,
}

/// How the server describes a posture without building it: a policy file
/// (`None` is the server's own), on a container runtime other than the one it
/// names where one is given.
pub type DescribeFactory =
    Arc<dyn Fn(Option<PathBuf>, Option<agent_core::worker::Runtime>) -> Where + Send + Sync>;

pub type AgencyFactory = Arc<
    dyn Fn(
            Option<PathBuf>,
            Option<agent_core::worker::Runtime>,
        ) -> futures_util::future::BoxFuture<'static, Result<Agency>>
        + Send
        + Sync,
>;

/// What this server resolved at startup, for the surface a person works in.
///
/// **Nothing here is new information.** Every field was decided before the
/// listener existed and printed to stderr, where a browser was never standing —
/// which is the whole reason the modal's first section exists and is read-only.
/// What a turn nobody approved may reach.
///
/// The gate shows what a plan *asks for*; this is the other term of the same
/// decision — what the turn in front of the panel can already do, and therefore
/// what approving actually adds. Derived here rather than in the page for the
/// reason `Agency` exists at all: a second resolution of a policy, in another
/// language, is a second sandbox, and the one on the page would be the one
/// somebody reads while deciding.
///
/// **There is no `writes` field.** The floor grants none by construction, so an
/// always-empty list would invite a reader to wonder when it is not; the panel
/// says so as a sentence instead. See
/// `RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Floor {
    /// What it may read, relative to the base where they are under it and `.`
    /// for the base itself — the spelling a policy file uses.
    reads: Vec<String>,
    /// What it may run. The session's allowlist, unchanged: subtracting it
    /// would take `rg` and `ls`, which is what exploration is for.
    commands: Vec<String>,
    /// Whether it may reach the network. The session's, unchanged.
    network: bool,
    /// Whether a write it refuses, and the policy file would allow, is put to
    /// the person watching — *allow once* — rather than handed to the model as
    /// a refusal. `false` where approvals must be signed, since a click is not
    /// one. See `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
    asks: bool,
}

impl Floor {
    /// From the floor a session derived once, as the page should print it.
    fn of(floor: &Sandbox, asks: bool) -> Self {
        let policy = floor.to_policy();
        Self {
            reads: policy
                .paths
                .iter()
                .map(
                    |rule| match workspace::relative_of(floor.base(), &rule.path) {
                        here if here.is_empty() => ".".to_string(),
                        under => under,
                    },
                )
                .collect(),
            commands: policy.commands.clone(),
            network: policy.network,
            asks,
        }
    }
}

/// See `RECORD/2026-09-07.configuring-from-the-browser.completed.md`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Settings {
    /// The profile that named this destination, when one did.
    profile: Option<String>,
    /// The backend's own name, which is what a record header carries.
    backend: String,
    /// Where it sends. Empty for the mock, which is nowhere.
    destination: String,
    remote: bool,
    model: String,
    window: Option<u32>,
    window_from: crate::provider::WindowFrom,
    /// Why the window may not be what the server is serving. Present only where
    /// the API has no field to send it in, which is the only place it can
    /// silently differ.
    window_caveat: Option<String>,
    reserve: u32,
    /// The three resend rules this session is rendering its window under, as
    /// the header records them. Here rather than only in the recording because
    /// a fact a session may *choose* and nothing can read back is a fact the
    /// page can contradict — the same reason `posture` is on this struct. The
    /// wire words are the header's, so a page and a recording of the same
    /// session say the arm the same way. See
    /// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`.
    repeat: agent_core::context::Repeat,
    prune: agent_core::context::Prune,
    results: agent_core::context::Results,
    /// What a draft or a plan this session opens is told about itself, the
    /// same reason `repeat`/`prune`/`results` are here: a fact a session may
    /// choose and nothing can read back is a fact the page can contradict.
    #[serde(
        default,
        skip_serializing_if = "crate::provider::AuthorityNotes::is_empty"
    )]
    authority: crate::provider::AuthorityNotes,
    counter: agent_core::context::Counter,
    counter_warning: Option<String>,
    select_tokens: u32,
    map_tokens: u32,
    sandbox: String,
    /// The directory `serve` was started in — the sandbox's base, as an
    /// absolute path.
    ///
    /// Display and *bounding*, never addressing: it is the ceiling the page's
    /// folder picker chooses a subdirectory of, and nothing is ever asked for
    /// by absolute path, because an absolute path in a URL is an invitation to
    /// send a different one. Filled at the route, from the live session's own
    /// sandbox, for the same reason `posture` is.
    #[serde(default)]
    base: String,
    /// What this session is allowed to do, by the name it was chosen under and
    /// the three facts a reader compares two runs on. Filled at the route from
    /// the *live* agency rather than carried in the destination: a posture is
    /// the session's, and a session can change it by starting another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    posture: Option<record::Posture>,
    /// What a turn nobody approved may reach. Filled at the route from the live
    /// agency, for the same reason `posture` and `base` are: it is the
    /// session's and not the destination's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    floor: Option<Floor>,
    store: Option<String>,
    /// Nothing has said where this run sends — see
    /// [`crate::provider::DestinationFrom`]. The page opens on the providers
    /// editor when it is set, and **never works this out for itself**: a
    /// browser deciding it from `backend == "mock"` is the same class of
    /// mistake as a second `is_this_machine` in JavaScript, and it would call a
    /// deliberate `--backend mock` run unconfigured.
    unconfigured: bool,
    /// Nobody chose this destination — it is the file's default, or the mock
    /// where there is none — so it follows the file: a new `default` moves an
    /// idle, empty live session at once, and a new session that names no
    /// provider reads the file again rather than inheriting. See
    /// [`crate::provider::Resolved::named`].
    /// How tool calls travel to this destination: `fenced` or `native`. See
    /// `RECORD/2026-09-30.native-tool-calls.completed.md`.
    tool_calls: crate::provider::ToolCalls,
    follows_default: bool,
}

impl Settings {
    /// The same run, pointed somewhere else.
    ///
    /// Every field a *destination* decides is replaced; every field the
    /// *process* decided — the sandbox, the map and selection budgets, the
    /// store — is kept, because a new session does not re-resolve those.
    fn pointed_at(
        &self,
        provider: &crate::provider::Resolved,
        budget: Budget,
        counter: agent_core::context::Counter,
        counter_warning: Option<String>,
    ) -> Self {
        Self {
            profile: provider.profile.clone(),
            backend: provider.kind.as_str().to_string(),
            destination: provider.url.clone(),
            remote: !provider.url.is_empty() && !crate::provider::is_this_machine(&provider.url),
            model: provider.model.clone(),
            window: budget.limit,
            window_from: provider.window_from,
            window_caveat: match provider.kind {
                crate::provider::BackendKind::Openai => {
                    agent_core::backend::openai::OpenAi::window_caveat(budget.limit)
                }
                _ => None,
            },
            reserve: budget.reserve,
            // From the budget this was handed rather than kept by `..self`:
            // they are neither the destination's nor the process's, and a
            // field carried by inheritance is one that is right by accident.
            repeat: budget.repeat,
            prune: budget.prune,
            results: budget.results,
            counter,
            counter_warning,
            unconfigured: provider.unconfigured(),
            follows_default: !provider.named,
            tool_calls: provider.tool_calls,
            ..self.clone()
        }
    }
}

pub struct StdioOptions {
    pub backend: Arc<dyn Backend>,
    /// The destination as `crate::provider` resolved it — the profile, the URL
    /// and whether it left this machine. `stdio` carries it for the same reason
    /// `serve` does: both build the same `App`, and only one of them serves a
    /// page that reads it.
    pub provider: crate::provider::Resolved,
    /// The sentence a run prints when it has no tokenizer, so the page can say
    /// what the terminal said.
    pub counter_warning: Option<String>,
    pub model: String,
    pub record: Option<PathBuf>,
    pub budget: Budget,
    pub counter: Arc<dyn TokenCounter>,
    /// The `--tokenizer` path, if one was given. Kept rather than only used,
    /// because a session that switches provider needs a counter for the *new*
    /// model and this is the only thing that answers how to build one.
    pub tokenizer: Option<PathBuf>,
    pub agency: Agency,
    /// How to build the agency for a posture a session names. `None` over
    /// stdio, where the process is the session and its policy file is a flag.
    pub agency_for: Option<AgencyFactory>,
    /// How to say where a posture would run without building it, for the
    /// terminal's environment picker. `None` where `agency_for` is.
    pub describe_for: Option<DescribeFactory>,
    /// `[posture.<name>]` out of `config.toml`, read once when this run
    /// started.
    pub postures: std::collections::BTreeMap<String, crate::provider::Posture>,
    /// Where the file is, for the page to name. `None` on a machine with no
    /// state directory.
    pub postures_path: Option<String>,
    /// The icon theme, already loaded. Loaded by the caller rather than here
    /// so that a failure to read it is reported where the other startup
    /// messages are, and so this file does not learn about `config.toml`.
    pub icons: Arc<crate::icons::Theme>,
    /// `[approvals]` from the same file the sandbox came from: who may approve,
    /// and whether anyone must sign to.
    pub approvers: Approvers,
    pub temperature: Option<f32>,
    pub seed: Option<u32>,
    /// Tokens of repository outline for the prefix. 0 is off — see
    /// `agent_core::repo_map`.
    pub map_tokens: u32,
    /// Which files that budget buys. Path order unless asked otherwise — see
    /// [`Order`], which carries the measurement that keeps it the default.
    pub map_order: Order,
    /// How the budget is packed from candidate outlines.
    pub map_fill: agent_core::repo_map::Fill,
    /// Tokens of selected fragments to fuse into each prompt. 0 is off. The
    /// map's opposite number: that block is the same every turn and is cached,
    /// this one is chosen from the turn's own text and is gone with it. See
    /// `RECORD/2026-09-06.selection-at-the-gate.completed.md`.
    pub select_tokens: u32,
    /// Which signals score a file, from the flags that switch them.
    pub select_weights: agent_core::select::Weights,
    /// `schema` or `grammar`, for every turn of the session — the same flag
    /// `chat` carries, unbuilt here until now. See
    /// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
    pub constrain: Option<crate::ConstrainKind>,
    /// Where sessions are cached between restarts.
    pub store: Option<PathBuf>,
}

impl App {
    /// The icon theme as it stands. A clone of the `Arc`, so no lock is held
    /// past this line.
    fn icons(&self) -> Arc<crate::icons::Theme> {
        self.icons
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Puts a new theme in place, one revision on from the one it replaces.
    fn swap_icons(&self, mut theme: crate::icons::Theme) {
        let mut icons = self
            .icons
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        theme.manifest.revision = icons.manifest.revision + 1;
        *icons = Arc::new(theme);
    }

    async fn create(options: StdioOptions) -> Result<Arc<Self>> {
        let StdioOptions {
            backend,
            provider,
            counter_warning,
            model,
            record,
            budget,
            counter,
            tokenizer,
            agency,
            agency_for,
            describe_for,
            postures,
            postures_path,
            icons,
            temperature,
            seed,
            map_tokens,
            map_order,
            map_fill,
            select_tokens,
            select_weights,
            constrain,
            store,
            approvers,
        } = options;
        let started_at = now_ms();

        // Built once, before the socket is up: the map is the last block of the
        // cached prefix, and a block rebuilt mid-session is not a prefix. What that
        // costs is named in `RECORD/2026-08-31.the-repo-map.completed.md`.
        let map = RepoMap::build_with(
            agency.sandbox.as_ref(),
            map_tokens,
            counter.as_ref(),
            map_order,
            map_fill,
        );
        if !map.is_empty() {
            eprintln!(
                "repository map — {} file(s), {} left out, {} of {map_tokens} tokens",
                map.files.len(),
                map.left_out,
                map.tokens,
            );
        }

        // Beside the map and not for the map's reason. The map is built once
        // because it is a *prefix* and a prefix rebuilt mid-session is not one;
        // this is the seed of a cache that is re-stamped every turn.
        //
        // It used to be built once and kept, and the comment here said a file
        // written mid-session was "not selectable until a restart". That was
        // the benign reading of the wrong behaviour: a file edited mid-session
        // stayed selectable at its *startup line numbers*, and the fragment
        // loader reads the current file at them — so the turn got the wrong
        // lines under the right path. See `rewalk_sources`.
        let walked = match select_tokens > 0 {
            true => {
                let walked = agent_core::repo_map::walk_sources(agency.sandbox.as_ref());
                eprintln!(
                    "selection — {} file(s) tagged, {select_tokens} tokens a turn",
                    walked.len()
                );
                walked
            }
            false => Vec::new(),
        };

        // No CLI flag reaches this, on purpose — see
        // `RECORD/2026-09-22.an-authority-a-model-is-told.completed.md` §What
        // was rejected. Read fresh rather than threaded through
        // `StdioOptions`, `machine_resend`'s own reason: a table the page
        // writes has to reach the next session, and this server's first one
        // is no exception. Loaded before the recorder so the very first line
        // of a `--record` file already carries it.
        let authority = machine_authority().await;
        let recorder = match record {
            Some(path) => Some(
                Recorder::create(
                    &path,
                    backend.name(),
                    &model,
                    budget,
                    counter.id(),
                    Some(agency.posture(None)),
                    &authority,
                    provider.tool_calls,
                    started_at,
                )
                .await?,
            ),
            None => None,
        };

        // Opened before answering, like the token: a store that cannot be
        // opened is a configuration error, and finding out four turns in means
        // four turns nobody can resume.
        let sessions = match &store {
            Some(path) => {
                let store = SessionStore::open(path)?;
                eprintln!("sessions — {}", path.display());
                Some(Mutex::new(store))
            }
            None => None,
        };

        let backend_name = backend.name().to_string();
        let model_name = model.clone();
        let counter_id = counter.id();
        let map_rendered = map.render();
        let settings = Settings {
            profile: provider.profile.clone(),
            backend: backend_name.clone(),
            destination: provider.url.clone(),
            // Read from the URL rather than from the profile's own word: the
            // declaration in the file answers *was this deliberate*, and this
            // field answers *where is it going*, which is a different question
            // and the one a person reading a page is asking.
            remote: !provider.url.is_empty() && !crate::provider::is_this_machine(&provider.url),
            model: model_name.clone(),
            window: budget.limit,
            window_from: provider.window_from,
            window_caveat: match provider.kind {
                crate::provider::BackendKind::Openai => {
                    agent_core::backend::openai::OpenAi::window_caveat(budget.limit)
                }
                _ => None,
            },
            reserve: budget.reserve,
            repeat: budget.repeat,
            prune: budget.prune,
            results: budget.results,
            authority: authority.clone(),
            counter: counter_id.clone(),
            counter_warning,
            select_tokens,
            map_tokens,
            sandbox: agency.describe(),
            // Both filled at the route, from whatever the live session is
            // running: a posture is the session's, and so is the sandbox whose
            // base the page roots its folder picker at.
            base: String::new(),
            posture: None,
            floor: None,
            store: store.as_ref().map(|path| path.display().to_string()),
            unconfigured: provider.unconfigured(),
            follows_default: !provider.named,
            tool_calls: provider.tool_calls,
        };
        // One `Arc` for the field and the header both: the posture line is a
        // fact about this agency, and building it after the move would need a
        // second one.
        let agency = Arc::new(agency);
        // Built once, here, for the same reason the map is: a constraint
        // compiled per turn would cost the compile every turn for a value
        // that is the same every time. Mirrors `chat`'s own handling exactly
        // — see `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
        let built = constrain
            .map(|kind| kind.build(&agency.tools))
            .transpose()?;
        if let Some(built) = &built
            && let Some(caveat) = backend.constrain_caveat(built)
        {
            eprintln!("note: {caveat}");
        }
        let (constraint, schema_retry) = match (constrain, built) {
            (Some(crate::ConstrainKind::Grammar), Some(grammar)) => (Some(grammar), None),
            (Some(crate::ConstrainKind::Schema), Some(Constraint::Schema(schema))) => (
                None,
                Some(SchemaRetry {
                    schema,
                    tool_names: agency.tools.names().collect(),
                }),
            ),
            _ => (None, None),
        };
        Ok(Arc::new(App {
            destination: RwLock::new(Arc::new(Destination {
                backend,
                model,
                budget,
                counter,
                settings,
                authority: authority.clone(),
                // `serve` sets it, where there is a supervisor to start one:
                // see [`Serving::run`]'s own warm-up.
                engine: None,
                tool_calls: provider.tool_calls,
            })),
            tokenizer,
            approvers,
            walked: Mutex::new(walked),
            select_tokens,
            select_weights,
            session: Mutex::new(Session {
                next_turn: 1,
                current: None,
                cancel: None,
                context: agency
                    .with_tools(AgentContext::new(SYSTEM), provider.tool_calls)
                    .with_map(&map_rendered),
                prefix: PrefixTracker::default(),
                pending: None,
                narrowed: None,
                held: None,
            }),
            events: broadcast::channel(1024).0,
            recorder,
            agency: RwLock::new(agency.clone()),
            posture: Mutex::new(None),
            agency_for,
            describe_for,
            postures: std::sync::RwLock::new(postures),
            postures_path,
            icons: std::sync::RwLock::new(icons),
            engines: Arc::default(),
            tasks: Arc::default(),
            inflight: Arc::default(),
            temperature,
            seed,
            constraint,
            schema_retry,
            view: Mutex::new({
                let mut view = SessionView::new(LIVE_SESSION, &backend_name, &model_name);
                view.started_at = started_at;
                // The same three fields the stream's first header carries, and
                // set here for the same reason `started_at` is: the live view
                // is not built by applying that line, so anything the fold
                // would take out of it has to be put in by hand or the two
                // disagree. `tests/store_parity.rs` is what notices.
                view.posture = Some(agency.posture(None));
                view.repeat = Some(budget.repeat);
                view.prune = Some(budget.prune);
                view.results = Some(budget.results);
                view.tool_calls = provider
                    .tool_calls
                    .native()
                    .then(|| provider.tool_calls.as_str().to_string());
                view
            }),
            session_started_at: Mutex::new(started_at),
            session_id: Mutex::new(session_id(started_at)),
            map_rendered,
            store: sessions,
            // The first line of a stream is what makes two runs comparable, so
            // a session's own stream starts with one exactly as a `--record`
            // file does.
            stream: Mutex::new(vec![crate::session::header(
                &backend_name,
                &model_name,
                budget,
                counter_id,
                Some(agency.posture(None)),
                &authority,
                provider.tool_calls,
                started_at,
            )]),
        }))
    }

    /// The live session's destination, as a cheap clone.
    ///
    /// Every reader takes one of these and drops the guard. Holding it across
    /// an `await` — which is most of what this file does — would be a deadlock
    /// against the one writer in `create_session`.
    async fn destination(&self) -> Arc<Destination> {
        self.destination.read().await.clone()
    }

    /// The postures as they are now. A clone, so no lock is held across
    /// whatever the caller does with them.
    fn postures(&self) -> std::collections::BTreeMap<String, crate::provider::Posture> {
        self.postures
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// What this session may do and where its tools run, as a cheap clone.
    ///
    /// Beside [`App::destination`] and for its reason: the guard is never held
    /// across an `await`, because the writer is `create_session` and the readers
    /// are every turn.
    async fn agency(&self) -> Arc<Agency> {
        self.agency.read().await.clone()
    }

    /// Publishes one event to every client and to the record, in that order.
    async fn publish(&self, event: Event) {
        if let Event::Protocol(message) = &event {
            log_event(message);
        }
        if let Some(recorder) = &self.recorder {
            recorder.write(&event);
        }
        {
            let at_ms = now_ms().saturating_sub(*self.session_started_at.lock().await);
            let mut view = self.view.lock().await;
            match &event {
                Event::Protocol(message) => view.apply_protocol(at_ms, message),
                Event::Trace(message) => view.apply_trace(at_ms, message),
            }
            if self.store.is_some() {
                self.stream
                    .lock()
                    .await
                    .push(crate::session::line(&event, at_ms));
            }
        }
        // At checkpoints, not per event. The store is a *cache* of the fold, so
        // it is allowed to lag the record — and writing the whole fold on every
        // token is quadratic in the length of a session. What it is never
        // allowed to do is contradict the record, which is why the checkpoints
        // are the moments the session's shape changed rather than a timer.
        if is_checkpoint(&event) {
            self.checkpoint().await;
        }

        // No subscribers is the ordinary state of a server nobody has opened yet.
        let _ = self.events.send(event);
    }

    /// Writes the fold as it stands. Failures are reported once and do not stop
    /// a turn: a session whose cache could not be written is still a session,
    /// and the record beside it is the account that matters.
    async fn checkpoint(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let stored = {
            let session_id = self.session_id.lock().await.clone();
            let view = self.view.lock().await;
            let mut stored = view.clone();
            // The id is the session's *name* rather than part of the fold —
            // `SessionView::from_record` takes it as an argument for the same
            // reason, and `luu export` takes it from the file stem.
            stored.id = session_id.clone();
            if stored.title == LIVE_SESSION {
                stored.title = session_id;
            }
            stored
        };
        // The stream before the fold, and never the other way round: the fold is
        // a cache of it, so a crash between the two must leave a store whose
        // fold lags its stream rather than one whose fold claims a line the
        // stream does not have.
        let queued: Vec<record::RecordLine> = self.stream.lock().await.drain(..).collect();
        let id = stored.id.clone();
        let mut store = store.lock().await;
        if let Err(error) = store.append(&id, &queued) {
            eprintln!("warning: could not write the session stream: {error:#}");
        }
        // The profile this session ran on, so the picker can start on the
        // model it was last given. `None` where nothing named a profile.
        let provider = self.destination().await.settings.profile.clone();
        if let Err(error) = store.save(&stored, provider.as_deref()) {
            eprintln!("warning: could not write the session store: {error:#}");
        }
    }
}

/// What a session is called on disk: when it started, then enough to keep two
/// of them apart.
///
/// Sortable and readable first, because the one thing a person listing sessions
/// wants is when it happened. Unique second, and it has to be: two servers
/// started in the same millisecond would otherwise be one row, and the second
/// would silently overwrite the first — a history that loses sessions is worse
/// than none, because nothing says it did.
fn session_id(started_at: u64) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("session-{started_at}-{:x}{n:x}", std::process::id())
}

/// Which events are worth a write: the ones after which the session is a
/// different shape. Tokens are not — a turn's text is only complete at
/// `ended`, and a cache of half a turn is a cache of nothing.
fn is_checkpoint(event: &Event) -> bool {
    match event {
        Event::Protocol(message) => matches!(
            message,
            ServerMessage::Ended { .. }
                | ServerMessage::Failed { .. }
                // The alternation's own three. A draft opening and a plan
                // reaching the table are both moments the session's shape
                // changed, and a server that died between a proposal and the
                // next turn used to come back with the question gone.
                | ServerMessage::DraftOpened { .. }
                | ServerMessage::PlanProposed { .. }
                | ServerMessage::PlanDeclined
                | ServerMessage::JobProposed { .. }
                | ServerMessage::JobApproved { .. }
                | ServerMessage::JobClosed { .. }
                | ServerMessage::JobRejected { .. }
                | ServerMessage::JobReopened { .. }
        ),
        Event::Trace(_) => false,
    }
}

pub struct ServeOptions {
    pub address: SocketAddr,
    pub backend: Arc<dyn Backend>,
    /// The destination as `crate::provider` resolved it, for the page that
    /// shows where this server is sending.
    pub provider: crate::provider::Resolved,
    /// The tokenizer warning, if the run had one to print.
    pub counter_warning: Option<String>,
    pub model: String,
    pub record: Option<PathBuf>,
    pub budget: Budget,
    pub counter: Arc<dyn TokenCounter>,
    /// The `--tokenizer` path, if one was given. See [`StdioOptions::tokenizer`].
    pub tokenizer: Option<PathBuf>,
    pub agency: Agency,
    /// How to build the agency for a posture a session names. `None` over
    /// stdio, where the process is the session and its policy file is a flag.
    pub agency_for: Option<AgencyFactory>,
    /// How to say where a posture would run without building it, for the
    /// terminal's environment picker. `None` where `agency_for` is.
    pub describe_for: Option<DescribeFactory>,
    /// `[posture.<name>]` out of `config.toml`, read once when this run
    /// started.
    pub postures: std::collections::BTreeMap<String, crate::provider::Posture>,
    /// Where the file is, for the page to name. `None` on a machine with no
    /// state directory.
    pub postures_path: Option<String>,
    /// The icon theme `[ui] icon-theme` named, already loaded. Only the
    /// browser surface has one: `stdio` gets an empty theme, because there
    /// is no page there to draw it.
    pub icons: Arc<crate::icons::Theme>,
    /// `[approvals]` from the same file the sandbox came from: who may approve,
    /// and whether anyone must sign to.
    pub approvers: Approvers,
    pub temperature: Option<f32>,
    pub seed: Option<u32>,
    /// Tokens of repository outline for the prefix. 0 is off — see
    /// `agent_core::repo_map`.
    pub map_tokens: u32,
    /// Which files that budget buys. Path order unless asked otherwise — see
    /// [`Order`], which carries the measurement that keeps it the default.
    pub map_order: Order,
    /// How the budget is packed from candidate outlines.
    pub map_fill: agent_core::repo_map::Fill,
    /// Tokens of selected fragments to fuse into each prompt. 0 is off.
    pub select_tokens: u32,
    /// Which signals score a file, from the flags that switch them.
    pub select_weights: agent_core::select::Weights,
    /// `schema` or `grammar`, for every turn of the session. See
    /// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
    pub constrain: Option<crate::ConstrainKind>,
    /// The file holding the bearer token this server requires, if any.
    /// `None` on a loopback address means no auth; `None` on any other
    /// address means [`bind`] refuses.
    pub auth_token_file: Option<PathBuf>,
    /// Where sessions are cached between restarts. `None` keeps the session in
    /// memory for the life of the process, which is what `serve` did before the
    /// store existed.
    pub store: Option<PathBuf>,
}

/// A server that has its port and has not started answering yet.
///
/// `serve` used to bind and serve in one call, which left a test no way to
/// learn which port an ephemeral bind got — and a test that binds a *fixed*
/// port is a test that fails whenever two jobs share a runner.
pub struct Serving {
    address: SocketAddr,
    listener: tokio::net::TcpListener,
    router: Router,
    app: Arc<App>,
}

impl Serving {
    /// The address actually bound, which is what a `:0` request resolved to.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Serves until SIGINT or SIGTERM, then stops every engine it started.
    ///
    /// A signal ends the process without running one `Drop`, so
    /// `kill_on_drop` alone would leave a model server holding its memory
    /// after `serve` is gone. A terminal's Ctrl-C reaches the whole process
    /// group anyway; a `kill` from a test harness reaches only this process.
    ///
    /// Not `with_graceful_shutdown`: that waits for every connection to close,
    /// and the page holds a WebSocket open for as long as it is on screen, so
    /// a Ctrl-C with the page open would never finish.
    pub async fn run(self) -> Result<()> {
        let engines = self.app.engines.clone();
        let result = tokio::select! {
            served = async { axum::serve(self.listener, self.router).await } => {
                served.context("serving")
            }
            () = asked_to_stop() => Ok(()),
        };
        engines.stop_all().await;
        result
    }
}

async fn asked_to_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let serving = bind(options).await?;
    println!("luu serve → http://{}", serving.address());
    serving.run().await
}

pub async fn bind(options: ServeOptions) -> Result<Serving> {
    let ServeOptions {
        address,
        backend,
        provider,
        counter_warning,
        model,
        record,
        budget,
        counter,
        tokenizer,
        agency,
        agency_for,
        describe_for,
        postures,
        postures_path,
        temperature,
        seed,
        map_tokens,
        map_order,
        map_fill,
        select_tokens,
        select_weights,
        constrain,
        auth_token_file,
        store,
        approvers,
        icons,
    } = options;
    // Before anything else, and before the listener exists: a port that would
    // publish task approval to the network is not a port this binds and then
    // warns about.
    let auth = Arc::new(crate::auth::resolve(&address, auth_token_file.as_deref())?);
    let starts = crate::engines::for_run(&provider);

    let app = App::create(StdioOptions {
        backend,
        provider,
        counter_warning,
        model,
        icons,
        record,
        budget,
        counter,
        tokenizer,
        agency,
        agency_for,
        describe_for,
        postures,
        postures_path,
        temperature,
        seed,
        map_tokens,
        map_order,
        map_fill,
        select_tokens,
        select_weights,
        constrain,
        store,
        approvers,
    })
    .await?;

    // The profile this server resolved names an engine: started now, in the
    // background, so the first turn is not the one that waits for a model to
    // load — and if it is, it waits on this rather than meeting a closed port,
    // because the backend is wrapped the way every switch wraps it. A failure
    // is said here and again by the turn that meets it.
    if let Some((name, engine)) = starts {
        let mut destination = app.destination.write().await;
        let current = destination.as_ref();
        *destination = Arc::new(Destination {
            backend: Arc::new(crate::engines::Started::new(
                current.backend.clone(),
                app.engines.clone(),
                name.clone(),
                engine.clone(),
            )),
            model: current.model.clone(),
            budget: current.budget,
            counter: current.counter.clone(),
            settings: current.settings.clone(),
            authority: current.authority.clone(),
            engine: Some((name.clone(), engine.clone())),
            tool_calls: current.tool_calls,
        });
        app.engines.warm(&name, &engine);
    }
    learn_window(app.clone());

    // Two halves, because they are two surfaces. `/ws` is authority and
    // `/api/*` is this session's prompts and source — both behind the token
    // when there is one. The embedded UI is not: it is the same bytes in every
    // copy of a public binary, and a browser navigating to a page cannot carry
    // an `Authorization` header, so gating it would only make the guarded
    // server unusable from the client written for it.
    let guarded = Router::new()
        .route("/ws", get(protocol_socket))
        .route("/ws/trace", get(trace_socket))
        // The read side. Every path also answers with a `.json` suffix, because
        // that is the only shape a static host can mirror — see `luu export`.
        //
        // Where the suffix sits on a parameter it is not a route of its own:
        // axum allows only one parameter per segment, so `{id}` captures
        // `completed-turn.json` whole and the handler strips it. Only the
        // literal segments get a second route.
        // What this server resolved, and the file that named it. The first is
        // read-only always; the second is writable only from this machine —
        // `PUT` outlives the session and the gate, and a bearer token answers
        // who may reach the port rather than who may decide that.
        .route("/api/settings", get(get_settings))
        .route("/api/settings.json", get(get_settings))
        .route("/api/providers", get(get_providers).put(put_providers))
        .route("/api/providers.json", get(get_providers))
        .route("/api/providers/{name}/models", get(get_provider_models))
        .route("/api/resend", get(get_resend).put(put_resend))
        .route("/api/authority", get(get_authority).put(put_authority))
        // The icon theme, chosen from Settings: what is set, what this machine
        // has, and a folder picked in the browser imported into the state
        // directory. The import is the one body on this API that is not
        // small, so it alone gets a larger limit.
        .route("/api/icon-theme", get(get_icon_theme).put(put_icon_theme))
        .route(
            "/api/icon-theme/import",
            post(import_icon_theme).layer(axum::extract::DefaultBodyLimit::max(
                crate::icons::IMPORT_MAX_BYTES + 1024 * 1024,
            )),
        )
        .route("/api/postures", get(get_postures).put(put_postures))
        .route("/api/postures.json", get(get_postures))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions.json", get(list_sessions))
        .route(
            "/api/sessions/{id}",
            get(get_session)
                .patch(rename_session)
                .delete(delete_session_handler),
        )
        .route("/api/sessions/{id}/resume", post(resume_session))
        .route("/api/sessions/{id}/turns", get(get_turns))
        .route("/api/sessions/{id}/turns.json", get(get_turns))
        .route("/api/sessions/{id}/turns/{turn}", get(get_turn))
        .route("/api/sessions/{id}/turns/{turn}/prompt", get(get_prompt))
        .route(
            "/api/sessions/{id}/turns/{turn}/prompt.json",
            get(get_prompt),
        )
        .route("/api/sessions/{id}/context", get(get_context))
        .route("/api/sessions/{id}/context.json", get(get_context))
        .route("/api/sessions/{id}/counts", get(get_counts))
        .route("/api/sessions/{id}/counts.json", get(get_counts))
        // The workspace, for the person rather than for the model: a file
        // tree, what git says changed, one file, one diff. Read-only and
        // deliberately not behind the job gate — see `crate::workspace`'s own
        // first paragraph for why, and note that the sandbox still bounds
        // every path.
        .route("/api/workspace/tree", get(get_workspace_tree))
        .route("/api/workspace/file", get(get_workspace_file))
        .route("/api/workspace/git-status", get(get_workspace_git_status))
        .route("/api/workspace/git-diff", get(get_workspace_git_diff))
        .route("/api/workspace/events", get(get_workspace_events))
        // A fenced block out of a reply, coloured by the same highlighter as a
        // file: the reply is already the page's, so this reads nothing on disk.
        .route("/api/highlight", post(post_highlight))
        // The icon theme this machine named, if it named one. `{id}` is an id
        // out of the manifest and never a path — see `crate::icons`.
        .route("/api/icons/manifest", get(get_icons_manifest))
        .route("/api/icons/{id}", get(get_icon))
        // Whether the optional editor is on this machine. A *question*, rather
        // than the page probing `/vendor/monaco/loader.js` and reading the
        // 404 — which works, and logs a console error on every visit of every
        // checkout that has no Monaco, which is most of them.
        .route("/api/monaco", get(get_monaco))
        // A shell in the live session's container, and whether there is one.
        // Under `/ws` so `?token=` reaches it, which is all a browser's
        // `WebSocket` can send. See [`terminal_allowed`] for what else it
        // asks, and `RECORD/2026-10-01.a-terminal-in-the-container.completed.md`.
        .route("/ws/terminal", get(terminal_socket))
        .route("/api/terminal", get(get_terminal).put(put_terminal))
        // What Settings → Runtimes shows and does: the runtimes probed, the
        // images they have, and a build of one that is missing. See
        // `crate::runtimes`.
        .route("/api/runtimes", get(get_runtimes))
        .route("/api/runtimes/tasks", get(get_tasks))
        .route("/api/runtimes/{runtime}/build", post(build_image))
        .route("/api/runtimes/{runtime}/install", post(install_runtime))
        .route("/api/runtimes/{runtime}/start", post(start_runtime))
        // Moves the live session to another posture, between jobs. See
        // [`put_session_posture`].
        .route(
            "/api/session/posture",
            axum::routing::put(put_session_posture),
        )
        // Model servers luu starts. Every write here is held to more than the
        // providers route: see [`engines_allowed`].
        .route("/api/models", get(get_models))
        .route("/api/engines", get(get_engines).put(put_engines))
        .route("/api/engine", get(live_engine))
        .route("/api/engines/install", post(install_engine))
        .route("/api/engines/{name}/start", post(start_engine))
        .route("/api/engines/{name}/stop", post(stop_engine))
        // Other machines' `luu serve`, reached through this one: the list, and
        // everything a page served under `/h/<name>/` asks of its API. See
        // `crate::hosts`, and [`hosts_allowed`] for who may.
        .route("/api/hosts", get(get_hosts).put(put_hosts))
        .route("/h/{name}/api/{*rest}", axum::routing::any(host_api))
        .route("/h/{name}/ws", get(host_socket))
        .route("/h/{name}/ws/{*rest}", get(host_socket))
        .layer(middleware::from_fn_with_state(auth.clone(), require_token));

    let router = guarded
        .route("/", get(|| serve_asset("index.html")))
        // Monaco, when this machine has it. Ungated for the same reason the
        // embedded UI is: it is a third-party editor, the same bytes in every
        // copy, and a `<script>` tag cannot carry an `Authorization` header.
        .route("/vendor/monaco/{*path}", get(monaco_asset))
        // xterm.js, the same way and for the same reasons.
        .route("/vendor/xterm/{*path}", get(xterm_asset))
        // This server's own page, under a host's prefix: every request it
        // makes is relative, so served here its API is the host's.
        .route("/h/{name}", get(host_page_bare))
        .route("/h/{name}/", get(|| serve_asset("index.html")))
        .route("/h/{name}/{*path}", get(host_page))
        .route("/{*path}", get(asset_handler))
        .with_state(AppRouterState {
            app: app.clone(),
            auth: auth.clone(),
            // Loopback only, and decided here because this is where the bound
            // address is known. Writing a `default` provider redirects every
            // future run on this machine, outside the gate and after the
            // session ends — a larger authority than approving one job, and
            // not one a bearer token should carry.
            providers_editable: address.ip().is_loopback(),
        })
        // Outermost, so it sees every request — the guarded half, the page's
        // own files, and a request the token check refused — and the time it
        // took is the time the client waited. See `crate::log`.
        .layer(middleware::from_fn_with_state(
            app.inflight.clone(),
            crate::log::requests,
        ));
    crate::log::Watchdog::start(app.inflight.clone());

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;
    // Not `address`: a `:0` request would print the port nobody can connect to.
    let address = listener.local_addr().context("the bound address")?;

    Ok(Serving {
        address,
        listener,
        router,
        app,
    })
}

#[derive(Clone)]
struct AppRouterState {
    app: Arc<App>,
    /// Whether this surface may rewrite `config.toml`. See where it is set.
    providers_editable: bool,
    /// What the port requires. `/ws` reads it for the same reason the
    /// middleware does: a port a network can reach is held to more than one a
    /// person's own browser can.
    auth: Arc<Auth>,
}

async fn asset_handler(uri: Uri) -> Response {
    serve_asset(uri.path().trim_start_matches('/')).await
}

async fn serve_asset(path: &str) -> Response {
    match Ui::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], file.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Where Monaco lives when somebody installed it.
///
/// **A node dependency of `web/`, not a payload in this tree.** It is
/// excluded from the `rust_embed` folder above, so a release binary does not
/// gain several megabytes of an editor most runs will never open, and it is
/// read from disk here instead. `LUU_UI_DIR` moves it, for a binary running
/// away from the checkout it was built in; without it the answer is the
/// checkout, which is where `luu serve` is run from in every case this panel
/// exists for.
///
/// A 404 is the honest answer for a checkout that ran `cargo build` and nothing
/// else, and it is the answer the page asks for before it offers the setting —
/// the same shape `[ui] icon-theme` has, where the feature waits to be told it
/// is there rather than shipping a copy of itself.
fn monaco_root() -> PathBuf {
    node_modules().join("monaco-editor/min/vs")
}

/// `web/node_modules`, where the page's optional dependencies are. See
/// [`monaco_root`] for why they are read from disk.
fn node_modules() -> PathBuf {
    let ui = std::env::var_os("LUU_UI_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web"));
    ui.join("node_modules")
}

/// Where xterm.js and its fit addon live, when somebody installed them: the
/// terminal panel's one dependency, optional the way Monaco is.
fn xterm_root() -> PathBuf {
    node_modules().join("@xterm")
}

/// Whether Monaco is installed, as an answer rather than as a missing file.
async fn get_monaco() -> Response {
    let installed = monaco_root().join("loader.js").is_file();
    Json(serde_json::json!({ "installed": installed })).into_response()
}

async fn monaco_asset(Path(path): Path<String>) -> Response {
    // Not installed. Nothing to say about it that the page does not already
    // handle by staying on its own viewer.
    installed_asset(monaco_root(), "monaco", &path).await
}

async fn xterm_asset(Path(path): Path<String>) -> Response {
    installed_asset(xterm_root(), "xterm.js", &path).await
}

/// One file under an optional dependency's directory, and never one outside it.
async fn installed_asset(root: PathBuf, what: &str, path: &str) -> Response {
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(_) => {
            return (StatusCode::NOT_FOUND, format!("{what} is not installed")).into_response();
        }
    };
    // Canonicalised and then checked for containment, rather than trusted after
    // a scan for `..`: the path arrives from a browser, and the only question
    // worth asking about it is where it actually lands. `/api/icons/{id}` takes
    // the stricter route for the same reason — there nothing client-supplied
    // reaches the filesystem at all.
    let Ok(asked) = root.join(path).canonicalize() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if !asked.starts_with(&root) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match tokio::fs::read(&asked).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&asked).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

// ---- the terminal ------------------------------------------------------------
//
// A shell in the live session's container, for the panel under the editor.
// See `crate::terminal` and `RECORD/2026-10-01.a-terminal-in-the-container.completed.md`.

/// Where a terminal opens, and the lines that open and end it.
struct Shell {
    /// `host` or `container`, as `/api/terminal` says it.
    place: &'static str,
    open: Vec<String>,
    /// The host shell's working directory: the sandbox's base, which is where
    /// the model's commands start too. `None` in a container, whose working
    /// directory the run already set.
    cwd: Option<PathBuf>,
    /// A container's second line — see `TerminalLine`. A host shell needs
    /// none: the PTY's hangup reaches it with no runtime in between.
    end: Option<Vec<String>>,
}

/// Where a terminal would open for the live session, or why it would not.
///
/// **Where the session's commands run**, and nowhere else: in its container
/// when it has one, on this machine when it does not (`host`, and `direct`,
/// whose worker is a child here). A host shell is the whole account, so
/// `[terminal] host` decides who gets one — by default a browser on this
/// machine, the rule [`engines_allowed`] keeps for starting a process here.
/// See `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
async fn terminal_shell(
    state: &AppRouterState,
    headers: &axum::http::HeaderMap,
) -> Result<Shell, String> {
    let agency = state.app.agency().await;
    if let Some(worker) = agency
        .worker
        .as_ref()
        .filter(|worker| worker.is_contained())
    {
        let line = worker.terminal_argv()?;
        return Ok(Shell {
            place: "container",
            open: line.open,
            cwd: None,
            end: Some(line.end),
        });
    }
    // A file that does not load is the default rule, not an open door: the
    // default is the narrow one.
    let rule = load_config()
        .await
        .map(|(config, _)| config.terminal().host)
        .unwrap_or_default();
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .unwrap_or("");
    match rule {
        crate::provider::HostShell::Never => {
            return Err(
                "[terminal] host = \"never\" in config.toml: this machine opens no \
                        shell on the host from the page"
                    .to_string(),
            );
        }
        crate::provider::HostShell::Loopback
            if !(state.providers_editable && host_is_loopback(host)) =>
        {
            return Err(format!(
                "this session runs on the host, and [terminal] host = \"loopback\" (the \
                 default) keeps a shell on the host to a browser on this machine — this \
                 request reached `{host}`. Set it to \"always\" in config.toml to offer it \
                 to whoever holds the token"
            ));
        }
        _ => {}
    }
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    Ok(Shell {
        place: "host",
        open: vec![shell, "-l".to_string()],
        cwd: Some(agency.sandbox.base().to_path_buf()),
        end: None,
    })
}

/// Whether the live session has a terminal to open and where, and whether
/// this machine has what draws one. Two questions, because their fixes are
/// different.
async fn get_terminal(
    headers: axum::http::HeaderMap,
    State(state): State<AppRouterState>,
) -> Response {
    let shell = terminal_shell(&state, &headers).await;
    let xterm = xterm_root().join("xterm/lib/xterm.mjs").is_file()
        && xterm_root().join("addon-fit/lib/addon-fit.mjs").is_file();
    let name = state.app.posture.lock().await.clone();
    let posture = state.app.agency().await.posture(name);
    Json(serde_json::json!({
        "available": shell.is_ok() && cfg!(unix),
        "place": shell.as_ref().ok().map(|shell| shell.place),
        "reason": shell.err(),
        "posture": posture,
        "xterm": xterm,
    }))
    .into_response()
}

/// Whether this upgrade may open a shell.
///
/// **`Origin` has to be this server.** A WebSocket is not covered by CORS:
/// any page in any tab can open one to `127.0.0.1:7878`, and on loopback with
/// no token nothing else would stand between that page and a shell. A browser
/// always sends `Origin` on an upgrade, and the page served from here sends
/// its own `Host` back as it, so the page passes and every other site does
/// not. A client with no `Origin` is refused too: nothing that is not this
/// page needs a terminal in it.
///
/// **And on an unguarded port, `Host` has to be loopback** — the rebinding
/// [`engines_allowed`] describes, where a site's own name resolves here and
/// its `Origin` then matches its `Host`.
fn terminal_allowed(
    state: &AppRouterState,
    headers: &axum::http::HeaderMap,
) -> Result<(), Box<Response>> {
    let refuse = |why: String| Box::new((StatusCode::FORBIDDEN, why).into_response());
    let text = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    };
    let (host, origin) = (text(header::HOST), text(header::ORIGIN));
    let from = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    if host.is_empty() || from != Some(host) {
        return Err(refuse(format!(
            "Origin `{origin}` is not this server (`{host}`), and only this server's page opens a terminal"
        )));
    }
    if !state.auth.is_token() && !host_is_loopback(host) {
        return Err(refuse(format!(
            "Host `{host}` is not this machine, and an unguarded port opens a terminal only to it"
        )));
    }
    Ok(())
}

async fn terminal_socket(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<AppRouterState>,
) -> Response {
    if let Err(refused) = terminal_allowed(&state, &headers) {
        return *refused;
    }
    // Decided before the upgrade, so a shell this request may not have is a
    // status and a sentence rather than a socket that opens and closes.
    let shell = match terminal_shell(&state, &headers).await {
        Ok(shell) => shell,
        Err(why) => return (StatusCode::CONFLICT, why).into_response(),
    };
    ws.on_upgrade(move |socket| async move {
        let id = SOCKETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        tracing::info!(target: "luu::ws", socket = id, channel = "terminal", place = shell.place, "open");
        run_terminal(socket, shell).await;
        tracing::info!(target: "luu::ws", socket = id, channel = "terminal", "closed");
    })
}

/// What the page sends that is not keystrokes.
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalControl {
    Resize { cols: u16, rows: u16 },
}

/// Keystrokes in as binary frames, output out as binary frames, a resize as
/// a text frame. Bytes rather than text both ways: a read can end in the
/// middle of a UTF-8 sequence, and the page's decoder is the one that knows
/// how to wait for the rest.
#[cfg(unix)]
async fn run_terminal(socket: WebSocket, shell: Shell) {
    let (mut sink, mut stream) = socket.split();
    let mut pty = match crate::terminal::Pty::spawn(&shell.open, shell.cwd.as_deref(), 80, 24) {
        Ok(pty) => pty,
        Err(error) => {
            let said = format!(
                "\r\nthe terminal could not start `{}`: {error}\r\n",
                shell.open.join(" ")
            );
            let _ = sink.send(WsMessage::Binary(said.into_bytes().into())).await;
            return;
        }
    };
    loop {
        tokio::select! {
            output = pty.output.recv() => match output {
                Some(bytes) => {
                    if sink.send(WsMessage::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                // The shell exited, or the container it was in went away.
                None => break,
            },
            incoming = stream.next() => match incoming {
                Some(Ok(WsMessage::Binary(bytes))) => pty.write(bytes.to_vec()),
                Some(Ok(WsMessage::Text(text))) => {
                    match serde_json::from_str::<TerminalControl>(&text) {
                        Ok(TerminalControl::Resize { cols, rows }) => pty.resize(cols, rows),
                        Err(error) => tracing::warn!(target: "luu::ws", %error, "terminal control"),
                    }
                }
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    pty.hang_up().await;
    let _ = sink.send(WsMessage::Close(None)).await;
    // The client is gone, and what it started in the container is not: see
    // `TerminalLine`. Bounded, because a runtime that hangs here would hold
    // nothing but this task.
    let Some(line) = shell.end.filter(|line| !line.is_empty()) else {
        return;
    };
    let mut end = tokio::process::Command::new(&line[0]);
    end.args(&line[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(10), end.status()).await {
        Ok(Ok(status)) if status.success() => {}
        outcome => tracing::warn!(
            target: "luu::ws",
            ?outcome,
            "the terminal's shell may still be running"
        ),
    }
}

#[cfg(not(unix))]
async fn run_terminal(socket: WebSocket, _shell: Shell) {
    let (mut sink, _) = socket.split();
    let said = "\r\nthe terminal needs a PTY, and this build has none\r\n";
    let _ = sink
        .send(WsMessage::Binary(said.as_bytes().to_vec().into()))
        .await;
}

/// The bearer check, on the control and read surfaces only.
///
/// Two ways to present the token, and the second one is a browser
/// concession rather than a preference: `Authorization: Bearer <token>` is
/// what `curl` and `fetch` send, and `?token=<token>` is accepted on `/ws`
/// because the browser's `WebSocket` constructor cannot set a header. It is
/// not accepted anywhere else — a query string is the part of a URL that ends
/// up in shell history and proxy logs, so the exception stays as narrow as the
/// thing that forces it.
async fn require_token(State(auth): State<Arc<Auth>>, request: Request, next: Next) -> Response {
    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string);
    let presented = match header {
        Some(token) => Some(token),
        None if is_socket_path(request.uri().path()) => {
            Query::<TokenQuery>::try_from_uri(request.uri())
                .ok()
                .and_then(|Query(query)| query.token)
        }
        None => None,
    };

    if !auth.admits(presented.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "this server requires a bearer token: --auth-token-file named one\n",
        )
            .into_response();
    }
    next.run(request).await
}

/// `/ws…`, or a host's `/h/<name>/ws…`: where `?token=` is accepted, because a
/// browser's `WebSocket` can send nothing else.
fn is_socket_path(path: &str) -> bool {
    path.starts_with("/ws")
        || path
            .strip_prefix("/h/")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, rest)| rest.starts_with("ws"))
}

#[derive(serde::Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// One protocol event in the log: its type and the few fields that say where a
/// session was when something went wrong — the turn, the job, how a turn
/// ended, which tool, what was refused, the error. Never a prompt, a token or a
/// tool's output: those are the session stream's, not a file kept for a week.
/// `token` is skipped outright, one line per word being no log at all. See
/// `crate::log`.
fn log_event(message: &ServerMessage) {
    if matches!(message, ServerMessage::Token { .. }) {
        return;
    }
    let Ok(value) = serde_json::to_value(message) else {
        return;
    };
    let field = |key: &str| {
        value.get(key).and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };
    let kind = field("type").unwrap_or_default();
    let (turn, job, reason, name) = (field("turn"), field("job"), field("reason"), field("name"));
    match kind.as_str() {
        "failed" | "refused" => tracing::warn!(
            target: "luu::session",
            event = kind,
            turn,
            request = field("request"),
            reason,
            detail = field("detail").or_else(|| field("message")),
            "session",
        ),
        _ => {
            tracing::info!(target: "luu::session", event = kind, turn, job, reason, tool = name, "session")
        }
    }
}

/// Sockets opened since `serve` started, so the log can pair an open with its
/// close. See `crate::log`.
static SOCKETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn protocol_socket(ws: WebSocketUpgrade, State(state): State<AppRouterState>) -> Response {
    ws.on_upgrade(move |socket| async move {
        let id = SOCKETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        tracing::info!(target: "luu::ws", socket = id, channel = "protocol", "open");
        run_protocol_socket(socket, state).await;
        tracing::info!(target: "luu::ws", socket = id, channel = "protocol", "closed");
    })
}

async fn trace_socket(ws: WebSocketUpgrade, State(state): State<AppRouterState>) -> Response {
    ws.on_upgrade(move |socket| async move {
        let id = SOCKETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        tracing::info!(target: "luu::ws", socket = id, channel = "trace", "open");
        run_trace_socket(socket, state.app).await;
        tracing::info!(target: "luu::ws", socket = id, channel = "trace", "closed");
    })
}

/// The trace channel is send-only: it explains the agent, it never drives it.
async fn run_trace_socket(socket: WebSocket, app: Arc<App>) {
    let (mut sink, mut stream) = socket.split();
    let mut events = app.events.subscribe();

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Trace(message)) => {
                    let Ok(json) = serde_json::to_string(&message) else { continue };
                    if sink.send(WsMessage::Text(json.into())).await.is_err() {
                        return;
                    }
                }
                Ok(Event::Protocol(_)) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            // Only to notice the client going away.
            incoming = stream.next() => if incoming.is_none() { return },
        }
    }
}

async fn run_protocol_socket(socket: WebSocket, state: AppRouterState) {
    let AppRouterState { app, auth, .. } = state;
    let (mut sink, mut stream) = socket.split();
    let mut events = app.events.subscribe();

    // A port that asks for a bearer token is a port a network can reach, and
    // there the client says what it speaks before it is allowed to say anything
    // else. On loopback the peer is this machine's own browser — the same
    // artifact as this binary — and requiring it would break every client
    // written before the handshake existed for a property the operating system
    // already gives.
    let must_greet = auth.is_token();
    let mut greeted = false;

    let hello = {
        let sending = app.destination().await;
        let session = app.session.lock().await;
        ServerMessage::Hello {
            protocol: protocol::VERSION,
            backend: sending.backend.name().to_string(),
            model: sending.model.clone(),
            turn: session.current,
            session: Some(app.session_id.lock().await.clone()),
        }
    };
    let Ok(json) = serde_json::to_string(&hello) else {
        return;
    };
    if sink.send(WsMessage::Text(json.into())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Protocol(message)) => {
                    let Ok(json) = serde_json::to_string(&message) else { continue };
                    if sink.send(WsMessage::Text(json.into())).await.is_err() {
                        return;
                    }
                }
                Ok(Event::Trace(_)) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            incoming = stream.next() => {
                let Some(Ok(WsMessage::Text(text))) = incoming else {
                    // A close, an error, or a frame we do not speak.
                    if matches!(incoming, None | Some(Err(_))) {
                        return;
                    }
                    continue;
                };
                let message = match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(message) => message,
                    // Unparseable input from one client must not take the
                    // server down for the others.
                    Err(error) => {
                        tracing::warn!(target: "luu::ws", %error, "a client message that does not parse");
                        continue;
                    }
                };
                // What was asked, by kind only: the prompt itself belongs in
                // the session stream, not in a file kept for a week.
                tracing::info!(
                    target: "luu::ws",
                    kind = serde_json::to_value(&message)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                        .unwrap_or_default(),
                    "client message",
                );
                match &message {
                    ClientMessage::Hello { protocol, format } => {
                        if let Err(detail) = handshake(*protocol, *format) {
                            close_with(&mut sink, version_refusal(detail)).await;
                            return;
                        }
                        greeted = true;
                        continue;
                    }
                    _ if must_greet && !greeted => {
                        close_with(
                            &mut sink,
                            version_refusal(
                                "this port requires a hello saying what the client speaks, \
                                 before anything else"
                                    .to_string(),
                            ),
                        )
                        .await;
                        return;
                    }
                    _ => {}
                }
                handle_client_message(&app, message).await;
            }
        }
    }
}

async fn handle_client_message(app: &Arc<App>, message: ClientMessage) {
    match message {
        // Answered by the transport, which is the only half that knows whether
        // this connection had to greet at all.
        ClientMessage::Hello { .. } => {}
        ClientMessage::Prompt { text } => {
            on_prompt(app.clone(), text).await;
        }
        ClientMessage::Cancel => {
            let session = app.session.lock().await;
            if let Some(cancel) = &session.cancel {
                let _ = cancel.send(true);
            }
        }
        ClientMessage::ApprovePlan {
            job,
            files,
            writes,
            commands,
            closes_on,
            network,
            enforcement,
            egress,
            signature,
        } => {
            approve_plan(
                app.clone(),
                job,
                files,
                writes,
                commands,
                closes_on,
                network,
                enforcement,
                egress,
                signature,
            )
            .await;
        }
        ClientMessage::DeclinePlan => {
            decline_plan(app.clone()).await;
        }
        ClientMessage::AnswerCall { turn, step, allow } => {
            let held = {
                let mut session = app.session.lock().await;
                match &session.held {
                    Some(held) if held.turn == turn && held.step == step => session.held.take(),
                    _ => None,
                }
            };
            match held {
                Some(held) => {
                    let _ = held.answer.send(allow);
                }
                None => {
                    refuse(
                        app,
                        "answer_call",
                        Refusal::Call,
                        format!("no call is held at turn {turn}, step {step}"),
                    )
                    .await;
                }
            }
        }
        ClientMessage::RequestPlan => {
            request_plan(app.clone()).await;
        }
        ClientMessage::CloseJob { job } => {
            close_job(app.clone(), job).await;
        }
        ClientMessage::ReopenJob { job } => {
            reopen_job(app.clone(), job).await;
        }
    }
}

/// Runs the agent protocol over standard input and standard output.
///
/// NDJSON lines of [`ClientMessage`] are read from `stdin`, and NDJSON lines
/// of [`ServerMessage`] are emitted to `stdout`. Logs and diagnostics are
/// printed to `stderr` so they do not corrupt the protocol stream.
pub async fn stdio(options: StdioOptions) -> Result<()> {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_stdio_stream(options, stdin, stdout).await
}

/// Serves the agent protocol over any asynchronous reader and writer.
pub async fn serve_stdio_stream<R, W>(options: StdioOptions, input: R, output: W) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let app = App::create(options).await?;
    serve_stdio(app, input, output).await
}

/// Speaks the protocol over `input` and `output` for an existing [`App`].
async fn serve_stdio<R, W>(app: Arc<App>, input: R, mut output: W) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mut lines = input.lines();
    let mut events = app.events.subscribe();

    let hello = {
        let sending = app.destination().await;
        let session = app.session.lock().await;
        ServerMessage::Hello {
            protocol: protocol::VERSION,
            backend: sending.backend.name().to_string(),
            model: sending.model.clone(),
            turn: session.current,
            session: Some(app.session_id.lock().await.clone()),
        }
    };
    let json = serde_json::to_string(&hello).context("serializing hello")?;
    output
        .write_all(json.as_bytes())
        .await
        .context("writing hello")?;
    output.write_all(b"\n").await.context("writing newline")?;
    output.flush().await.context("flushing hello")?;

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Protocol(message)) => {
                    let Ok(json) = serde_json::to_string(&message) else { continue };
                    if output.write_all(json.as_bytes()).await.is_err() {
                        return Ok(());
                    }
                    if output.write_all(b"\n").await.is_err() {
                        return Ok(());
                    }
                    if output.flush().await.is_err() {
                        return Ok(());
                    }
                }
                Ok(Event::Trace(_)) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            incoming = lines.next_line() => {
                match incoming {
                    Ok(Some(line)) => {
                        let text = line.trim();
                        if text.is_empty() {
                            continue;
                        }
                        let message = match serde_json::from_str::<ClientMessage>(text) {
                            Ok(message) => message,
                            Err(_) => continue,
                        };
                        // Checked when it comes and never required: the peer
                        // here is the process that spawned this one, and a
                        // subprocess bridge has no port for anyone else to
                        // reach. A mismatch still ends the conversation, in the
                        // one direction stdio has to say so.
                        if let ClientMessage::Hello { protocol, format } = &message {
                            if let Err(detail) = handshake(*protocol, *format) {
                                let refusal = version_refusal(detail);
                                if let Ok(json) = serde_json::to_string(&refusal) {
                                    let _ = output.write_all(json.as_bytes()).await;
                                    let _ = output.write_all(b"\n").await;
                                    let _ = output.flush().await;
                                }
                                return Ok(());
                            }
                            continue;
                        }
                        handle_client_message(&app, message).await;
                    }
                    Ok(None) | Err(_) => {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Says no, and why.
///
/// Every early return in this file that a client could not otherwise
/// distinguish from a dropped message goes through here. See
/// `RECORD/2026-08-30.a-refusal-is-a-message.completed.md`.
/// Whether a client that says what it speaks may be spoken to.
///
/// A mismatch in either direction: `VERSION` is bumped when a change would
/// break an older client, so a newer client is the case this host cannot parse
/// and an older one is the case it cannot. Neither side can repair it, so
/// neither side guesses.
fn handshake(protocol: u32, format: u32) -> Result<(), String> {
    if protocol != protocol::VERSION {
        return Err(format!(
            "this host speaks protocol {} and the client speaks {protocol}",
            protocol::VERSION
        ));
    }
    // 0 is a client that did not say, which is every client that only reads the
    // wire and never a recording.
    if format != 0 && format != record::FORMAT {
        return Err(format!(
            "this host writes record format {} and the client reads {format}",
            record::FORMAT
        ));
    }
    Ok(())
}

/// Says it, then closes — with the handshake a close has, rather than by
/// dropping the socket: a client that is told *why* over a connection that then
/// resets has to guess whether the reset was the answer.
async fn close_with(
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    refusal: ServerMessage,
) {
    if let Ok(json) = serde_json::to_string(&refusal) {
        let _ = sink.send(WsMessage::Text(json.into())).await;
    }
    let _ = sink.send(WsMessage::Close(None)).await;
}

/// The refusal that closes a connection, sent to the one client it is about.
///
/// Not `refuse`: a version mismatch is a property of this socket and not of the
/// session, and broadcasting it would tell every other client that something
/// they cannot act on happened.
fn version_refusal(detail: String) -> ServerMessage {
    ServerMessage::Refused {
        request: "hello".to_string(),
        reason: Refusal::Version,
        detail,
    }
}

async fn refuse(app: &Arc<App>, request: &str, reason: Refusal, detail: impl Into<String>) {
    app.publish(Event::Protocol(ServerMessage::Refused {
        request: request.to_string(),
        reason,
        detail: detail.into(),
    }))
    .await;
}

/// A prompt always belongs to a job, and runs.
///
/// **The gate is no longer here.** Before item 22 a prompt with nothing open
/// bought a planning call and was then held until a person answered it; under
/// the alternation there is always a job to land in — a draft, if nothing else
/// is open — so the prompt runs, and the gate is opened by
/// [`request_plan`] or by the model suggesting one. See
/// `RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md` §third.
async fn on_prompt(app: Arc<App>, prompt: String) {
    // A prompt arriving while a plan is on the table **is** the answer *more
    // changes*, which is the same answer as decline: the person is typing the
    // change they want instead of pressing a button. Refusing it here would be
    // the old `Pending` refusal outliving the held prompt it protected, and
    // making somebody approve a plan before they may speak is the gate running
    // the conversation.
    let declined = {
        let mut session = app.session.lock().await;
        match session.pending.take() {
            Some(pending) => {
                session
                    .context
                    .decline_plan(pending.objective, pending.plan, pending.source);
                true
            }
            None => false,
        }
    };
    if declined {
        app.publish(Event::Protocol(ServerMessage::PlanDeclined))
            .await;
    }

    start_turn(app, prompt).await;
}

/// Asks the agent for a plan over the draft as it stands.
///
/// **The explicit door**, beside the model's own suggestion, and the reason the
/// automatic one could be retired. The planning call is a turn: a prompt goes
/// in, tokens come out, it costs a window, and every panel that explains a turn
/// explains this one too. It is the one turn that is **not** remembered — what
/// survives it is the plan, and a plan block in the history would be paid for
/// on every later call.
///
/// What changed under the alternation is not the call but **what it reads**. It
/// used to answer over a held prompt, before anything had been looked at, which
/// is a 7B's worst case: plan work you have not seen. Now it runs over the open
/// draft — *the plan is the compaction of the draft* — so it is the call in
/// this system with the most context rather than the least. Nothing here
/// measures that; it is a row, not a claim.
///
/// It runs through [`run_turn`] rather than the agent loop, so it has no tools:
/// a planning call that could execute something would be the gate leaking.
async fn request_plan(app: Arc<App>) {
    // Nothing to compact, and nothing to plan over. A person who asks for a
    // plan before saying anything is asking the model to invent the objective,
    // which is the case the held prompt used to cover and nothing should now.
    {
        let session = app.session.lock().await;
        if session.pending.is_some() {
            drop(session);
            refuse(
                &app,
                "request_plan",
                Refusal::Pending,
                "a plan is already on the table",
            )
            .await;
            return;
        }
        if session.context.current_job().is_none() {
            drop(session);
            refuse(
                &app,
                "request_plan",
                Refusal::Job,
                "nothing has been asked yet, so there is no draft to plan over",
            )
            .await;
            return;
        }
    }

    // The objective of the draft being compacted, which is what the plan is
    // called if the model does not name one of its own.
    let objective = {
        let session = app.session.lock().await;
        session
            .context
            .current_job()
            .and_then(|job| session.context.job(job))
            .map(|job| job.objective.clone())
            .unwrap_or_default()
    };

    let Some((turn, cancel_rx, request, _code)) =
        begin_turn(&app, PLAN_OVER_DRAFT, Some(PLANNING)).await
    else {
        return;
    };

    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel(256);
        let forwarder = {
            let app = app.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if let Some(message) = ServerMessage::from_turn_event(turn, event) {
                        app.publish(Event::Protocol(message)).await;
                    }
                }
            })
        };
        let sending = app.destination().await;
        let outcome = run_turn(sending.backend.as_ref(), request, tx, cancel_rx).await;
        let _ = forwarder.await;

        {
            let mut session = app.session.lock().await;
            session.current = None;
            session.cancel = None;
            session.held = None;
        }

        // Cancelled or broken: there is no proposal, and inventing one would
        // put a plan in front of a person that nothing produced.
        if outcome.error.is_some() || outcome.reason == EndReason::Cancelled {
            return;
        }

        // A small model answering in prose is the ordinary case, and it must
        // not cost the gate. Then the proposal is the draft's own objective,
        // declaring nothing, and the panel says the model did not declare a
        // plan.
        //
        // Which of the two happened travels with the proposal. It is the gate's
        // headline number — how often a 7B plans at all — and the panel used to
        // infer it from an empty plan, which cannot tell a model that answered
        // in prose from one that declared an empty list.
        let (proposal, source) = match parse_plan(&outcome.text) {
            Some(proposal) => (proposal, PlanSource::Model),
            None => (
                Proposal {
                    objective: objective.clone(),
                    plan: Plan::default(),
                },
                PlanSource::Prose,
            ),
        };

        offer(&app, proposal, source).await;
    });
}

/// Puts a plan on the table, from either door.
///
/// One function because the two doors differ only in what produced the plan,
/// and the thing a person then sees must not: a plan the model volunteered and
/// a plan somebody asked for are answered by the same two buttons, and
/// `PlanSource` is where the difference is recorded.
async fn offer(app: &Arc<App>, proposal: Proposal, source: PlanSource) {
    {
        let mut session = app.session.lock().await;
        // Nothing to offer inside. A suggestion that arrives after the draft
        // closed belongs to no draft, and a second one while a plan is already
        // up would replace a question a person is in the middle of answering.
        //
        // And a plan job is not a draft: work that was approved does not get to
        // propose more of itself. A model that emits a plan block mid-job is
        // answering in the format it was taught, not asking for anything, and
        // putting that in front of a person as a gate would make the gate
        // meaningless by firing where nothing was decided.
        let drafting = session
            .context
            .current_job()
            .and_then(|job| session.context.job(job))
            .is_some_and(|job| job.is_draft());
        if session.pending.is_some() || !drafting {
            return;
        }
        session.pending = Some(Pending {
            objective: proposal.objective.clone(),
            plan: proposal.plan.clone(),
            source,
        });
    }
    app.publish(Event::Protocol(ServerMessage::PlanProposed {
        objective: proposal.objective,
        plan: proposal.plan,
        source: Some(source),
    }))
    .await;
}

/// Approves the plan on the table — with whatever the person added to it —
/// which **closes the open draft and opens the job that plan describes**.
///
/// Approving *is* closing, so this is one transition and not two: the draft
/// folds here and its summary becomes the new job's context, in the order a
/// reader wants it — this is what we looked at, this is what we decided, this
/// is what we are now doing.
///
/// The amendment is checked against the policy file exactly as the model's plan
/// was: an entry the file does not grant is dropped rather than approved, and
/// the plan that comes back on `job_approved` is what was actually approved.
/// That is the whole feedback: a client that adds a path nobody may touch sees
/// it missing from the plan it gets back, rather than being told nothing.
#[allow(clippy::too_many_arguments)]
async fn approve_plan(
    app: Arc<App>,
    signed_for: Option<JobId>,
    files: Vec<String>,
    writes: Vec<String>,
    commands: Vec<String>,
    closes_on: Option<String>,
    network: Option<bool>,
    enforcement: Option<agent_core::sandbox::Enforcement>,
    egress: Option<Vec<String>>,
    signature: Option<Signature>,
) {
    // The id this approval is about to hand out. Computed before the signature
    // is checked because that is what the signature is bound to: an approval
    // names no job on the wire any more, but it is still an approval *of* a
    // particular job in a particular session, and a grant that could be
    // replayed against a different one is not bound to anything.
    //
    // It is the id the approval will assign, not one that exists: closing the
    // draft does not add a job, so nothing between here and the push can move
    // it.
    let job = {
        let session = app.session.lock().await;
        match session.pending.is_some() {
            true => session.context.next_job_id(),
            false => {
                drop(session);
                refuse(
                    &app,
                    "approve_plan",
                    Refusal::Job,
                    "no plan is on the table",
                )
                .await;
                return;
            }
        }
    };
    // A signature is bound to a job id, so a client that signed for a different
    // one signed for something this session is not about to open — which is the
    // replay the binding exists to prevent. Checked before the signature rather
    // than inside it, so the refusal says which of the two went wrong.
    if let Some(signed_for) = signed_for
        && signed_for != job
    {
        refuse(
            &app,
            "approve_plan",
            Refusal::Job,
            format!("approval names job {signed_for}; the next job is {job}"),
        )
        .await;
        return;
    }

    // Before the state machine, and before anything is closed or opened: an
    // approval nobody can prove the authorship of is not an approval, and the
    // draft stays exactly as it was.
    let approved_by = {
        let session = app.session_id.lock().await;
        let approval = Approval {
            session: &session,
            job,
            files: &files,
            writes: &writes,
            commands: &commands,
            closes_on: closes_on.as_ref(),
            network,
            egress: egress.as_ref(),
            enforcement,
        };
        app.approvers.admits(&approval, signature.as_ref())
    };
    let approved_by = match approved_by {
        Ok(by) => by,
        Err(error) => {
            refuse(
                &app,
                "approve_plan",
                Refusal::Signature,
                format!("job {job}: {error}"),
            )
            .await;
            return;
        }
    };

    let approved = {
        let mut session = app.session.lock().await;
        // Nothing may be running: approving mid-turn would fold a draft out
        // from under a turn that is still writing into it.
        match session.current.is_none() && session.pending.is_some() {
            true => {
                let pending = session.pending.take().expect("checked just above");
                let (granted, mut dropped) = permitted(
                    &app.agency().await.sandbox,
                    files,
                    writes,
                    commands,
                    network,
                    egress,
                    enforcement,
                );
                // Checked against the plan as it will *be* rather than against
                // the amendment alone: the person types `closes_on` for a
                // command the model already declared more often than for one
                // they are adding in the same breath.
                let (closes_on, refused) = closing_condition(
                    &app.agency().await.sandbox,
                    Some(&pending.plan),
                    &granted,
                    closes_on,
                );
                dropped.extend(refused);
                // Amended on the proposal itself rather than through the
                // context: the plan is not stored anywhere until the approval
                // stores it, which is what *an id is what approval hands out*
                // costs and buys.
                let mut plan = pending.plan.clone();
                plan.amend(
                    &granted.files,
                    &granted.writes,
                    &granted.commands,
                    closes_on.as_deref(),
                    network.map(|_| granted.network),
                    Some(&granted.egress),
                    granted.enforcement,
                );
                let counter = app.destination().await.counter.clone();
                let approved = session.context.approve_plan(
                    pending.objective.clone(),
                    pending.plan,
                    plan.clone(),
                    pending.source,
                    approved_by.clone(),
                    counter.as_ref(),
                );
                debug_assert_eq!(approved.job, job, "the signature is bound to this id");
                // The draft's own sandbox was never narrowed, so there is
                // nothing to drop here — the narrowing below is the first one
                // this pair of jobs has had.
                Some((approved, pending.objective, plan, dropped))
            }
            // An approval for something that is no longer on the table, or a
            // second one. Not an error — two clients watching the same session
            // can both press it — but not silence either: the second one's
            // button did nothing and this is what says so.
            false => None,
        }
    };
    let Some((approved, objective, plan, dropped)) = approved else {
        refuse(
            &app,
            "approve_plan",
            Refusal::Job,
            "no plan is on the table, or a turn is running",
        )
        .await;
        return;
    };

    // The draft closed on the way in, and its fold is announced before the job
    // it opened: the two messages are one transition and a client that drew
    // them the other way round would show work starting inside a draft that is
    // still open.
    if let Some((draft, summary)) = approved.folded.clone() {
        let replaced = {
            let session = app.session.lock().await;
            session.context.replaced_by(draft)
        };
        app.publish(Event::Protocol(ServerMessage::JobClosed {
            job: draft,
            summary,
            // The same value `approve_plan` wrote on the job itself: the line
            // and the stored job are read by the same page, one live and one
            // after a reload, and a panel that says different things in the two
            // is the defect `the-panel-reads-the-pruned-lines` found twice.
            by: Some(ClosedBy::Approval),
            replaced,
        }))
        .await;
    }

    // What the person added that the policy file does not grant. It was left
    // out of the plan, and until now that was the whole feedback: a path
    // missing from a message nobody reads that closely.
    if !dropped.is_empty() {
        refuse(
            &app,
            "approve_plan",
            Refusal::NotGranted,
            format!(
                "approved without what the sandbox policy does not grant: {}",
                dropped.join("; "),
            ),
        )
        .await;
    }

    // The job's own sandbox, from here until it closes. A plan that names
    // nothing narrows to nothing, which is the point: a turn inside a job may
    // touch what the job was approved for.
    let narrowed = match plan.narrow(app.agency().await.sandbox.as_ref(), job) {
        Ok(sandbox) => Some(Arc::new(sandbox)),
        // Only a path that stopped existing between the check and here can do
        // this. Falling back to the session's sandbox would silently un-narrow
        // the job, so the job runs with nothing granted and the denials say
        // which plan refused.
        Err(error) => {
            eprintln!("job {job}: the approved plan could not be resolved: {error}");
            None
        }
    };
    {
        let mut session = app.session.lock().await;
        session.narrowed = narrowed.map(|sandbox| (job, sandbox));
    }

    app.publish(Event::Protocol(ServerMessage::JobApproved {
        job,
        from: approved.folded.map(|(draft, _)| draft),
        objective,
        plan,
        approved_by: Some(approved_by),
    }))
    .await;
    // No held prompt to run. The work starts on the next prompt, which lands in
    // the job this approval just opened — and the draft that preceded it is
    // already a summary by then.
}

/// The half of an amendment the policy file grants, and the half it does not —
/// which is what the refusal beside it is made of.
///
/// The person at the gate widens a plan up to the file and not past it —
/// otherwise the gate is the policy and `luu.toml` is a suggestion.
fn permitted(
    sandbox: &Sandbox,
    files: Vec<String>,
    writes: Vec<String>,
    commands: Vec<String>,
    network: Option<bool>,
    egress: Option<Vec<String>>,
    enforcement: Option<agent_core::sandbox::Enforcement>,
) -> (Plan, Vec<String>) {
    let asked = Plan {
        tasks: Vec::new(),
        files,
        writes,
        commands,
        // Not here: a closing condition is checked against the merged plan,
        // which this function has never seen. See `closing_condition`.
        closes_on: None,
        network: network.unwrap_or(false),
        egress: egress.unwrap_or_default(),
        enforcement,
    };
    let refused = asked.unmet(sandbox);
    let keep = |item: &String, kind: &str| {
        !refused
            .iter()
            .any(|line| line.starts_with(&format!("{kind} {item}:")))
    };
    let keep_egress = |domain: &str| {
        !refused
            .iter()
            .any(|line| line.starts_with(&format!("egress domain `{domain}`:")))
    };
    let granted_network = match network {
        Some(true) => sandbox.network(),
        Some(false) => false,
        None => false,
    };
    let granted = Plan {
        tasks: Vec::new(),
        files: asked
            .files
            .iter()
            .filter(|f| keep(f, "file"))
            .cloned()
            .collect(),
        writes: asked
            .writes
            .iter()
            .filter(|f| keep(f, "write"))
            .cloned()
            .collect(),
        commands: asked
            .commands
            .iter()
            .filter(|c| keep(c, "command"))
            .cloned()
            .collect(),
        closes_on: None,
        network: granted_network,
        // Dropped rather than downgraded when it was refused: a person who
        // asked to loosen enforcement and got the strict value anyway has been
        // told, in `dropped`, and the plan keeps the answer it already had.
        enforcement: enforcement
            .filter(|_| !refused.iter().any(|line| line.starts_with("enforcement "))),
        egress: asked
            .egress
            .iter()
            .filter(|e| keep_egress(e))
            .cloned()
            .collect(),
    };
    (granted, refused)
}

/// The closing condition the person typed, if the job will actually be able to
/// run it — and the one line that says why not, when it will not.
///
/// Checked against the *merged* plan: what the model declared, plus what the
/// gate is adding in the same approval, intersected with what the policy file
/// allows. A condition naming a command the job may not run can never be met,
/// and a job that can never close is worse than one closed by hand, because it
/// looks like it will close itself.
fn closing_condition(
    sandbox: &Sandbox,
    existing: Option<&Plan>,
    granted: &Plan,
    closes_on: Option<String>,
) -> (Option<String>, Option<String>) {
    let Some(closes_on) = closes_on
        .map(|it| it.trim().to_string())
        .filter(|it| !it.is_empty())
    else {
        return (None, None);
    };

    let mut commands: Vec<String> = existing.map(|p| p.commands.clone()).unwrap_or_default();
    for command in &granted.commands {
        if !commands.contains(command) {
            commands.push(command.clone());
        }
    }
    let merged = Plan {
        tasks: Vec::new(),
        files: Vec::new(),
        writes: Vec::new(),
        // What the plan may really run: `narrow` drops a declared command the
        // policy file never granted, silently, so checking against the
        // declaration alone would accept a condition the sandbox will deny.
        commands: commands
            .into_iter()
            .filter(|c| sandbox.commands().iter().any(|allowed| allowed == c))
            .collect(),
        closes_on: Some(closes_on.clone()),
        network: false,
        egress: Vec::new(),
        // This plan exists only to check one closing condition against the
        // commands, so it declares nothing else — including this.
        enforcement: None,
    };
    match merged
        .unmet(sandbox)
        .into_iter()
        .find(|line| line.starts_with("closes_on "))
    {
        Some(refusal) => (None, Some(refusal)),
        None => (Some(closes_on), None),
    }
}

/// Turns down the plan on the table. **Nothing closes and nothing folds.**
///
/// *Decline* and *more changes* are the same answer — not yet — and both keep
/// you in the job you are in, which under the alternation is the draft the plan
/// was offered inside. The held prompt that this used to drop on the floor does
/// not exist any more: prompts run when they arrive, so there is nothing here
/// to discard and the conversation simply continues.
///
/// The round is kept, because how many times a plan was put up before one was
/// approved is the number this event exists to produce.
async fn decline_plan(app: Arc<App>) {
    {
        let mut session = app.session.lock().await;
        let Some(pending) = session.pending.take() else {
            drop(session);
            refuse(
                &app,
                "decline_plan",
                Refusal::Job,
                "no plan is on the table",
            )
            .await;
            return;
        };
        session
            .context
            .decline_plan(pending.objective, pending.plan, pending.source);
    }
    app.publish(Event::Protocol(ServerMessage::PlanDeclined))
        .await;
}

/// Closes it: from here its turns are sent as their summary.
async fn close_job(app: Arc<App>, job: JobId) {
    let summary = {
        let mut session = app.session.lock().await;
        // Not while a turn is in flight: it would fold the history under the
        // turn that is being answered against it.
        if let Some(running) = session.current {
            drop(session);
            refuse(
                &app,
                "close_job",
                Refusal::Busy,
                format!("turn {running} is running; closing would fold the history under it"),
            )
            .await;
            return;
        }
        // Not while a plan is on the table either. The proposal was offered
        // *inside* the open draft, and closing that draft would take the gate
        // off the screen with the question still up — the same defect the old
        // shape had one state along, where closing a proposal left the session
        // with a held prompt and no way to answer it.
        if session.pending.is_some() && session.context.current_job() == Some(job) {
            drop(session);
            refuse(
                &app,
                "close_job",
                Refusal::Pending,
                format!("a plan is on the table, offered inside job {job}"),
            )
            .await;
            return;
        }
        let counter = app.destination().await.counter.clone();
        let summary = session.context.close_job(job, counter.as_ref());
        // The job's sandbox goes with the job. Outside one, the policy file
        // is the whole answer again — and the next prompt opens a draft under
        // it, which is where the alternation picks up.
        if summary.is_some() && session.narrowed.as_ref().is_some_and(|(id, _)| *id == job) {
            session.narrowed = None;
        }
        // Read inside the same lock as the close that wrote it.
        summary.map(|text| (text, session.context.replaced_by(job)))
    };
    let Some((summary, replaced)) = summary else {
        refuse(
            &app,
            "close_job",
            Refusal::Job,
            format!("job {job} is not open"),
        )
        .await;
        return;
    };

    app.publish(Event::Protocol(ServerMessage::JobClosed {
        job,
        summary,
        by: Some(ClosedBy::User),
        replaced,
    }))
    .await;
}

async fn reopen_job(app: Arc<App>, job: JobId) {
    // Taken before the session lock, because the two are never held in the same
    // order anywhere else either.
    let reopening = app.agency().await;
    {
        let mut session = app.session.lock().await;
        if let Some(running) = session.current {
            drop(session);
            refuse(
                &app,
                "reopen_job",
                Refusal::Busy,
                format!("turn {running} is running"),
            )
            .await;
            return;
        }
        if !session.context.reopen_job(job) {
            // Which of the two reasons, because they send a person to different
            // places. `reopen_job` takes only the last job — reopening an
            // earlier one would either leave two open or oblige this to close
            // what came after — and under the alternation that is the case
            // people actually hit, since every prompt opens a draft. Saying
            // *not closed* there is telling them something false about a job
            // they just watched fold. See
            // `RECORD/2026-09-21.the-alternation-on-the-page.completed.md`.
            // Which of the two reasons, because they send a person to different
            // places. `reopen_job` takes only the last job — reopening an
            // earlier one would either leave two open or oblige this to close
            // what came after — and under the alternation that is the case
            // people actually hit, since every prompt opens a draft. Saying
            // *not closed* there is telling them something false about a job
            // they just watched fold. See
            // `RECORD/2026-09-21.the-alternation-on-the-page.completed.md`.
            let last = session.context.jobs().last().map(|job| job.id);
            let detail = match last {
                Some(last) if last != job => format!(
                    "job {job} is not the last one: job {last} was opened after it, \
                     and reopening an earlier job would leave two open"
                ),
                _ => format!("job {job} is not closed"),
            };
            drop(session);
            refuse(&app, "reopen_job", Refusal::Job, detail).await;
            return;
        }
        // Live again, so its plan is the authority again. Rebuilt rather than
        // remembered: the sandbox is a resolution of the plan, and the plan is
        // what the session keeps.
        let agency = reopening;
        // A draft has no plan and therefore no narrowing — reopening one puts
        // its turns back in the window verbatim and puts what may be touched
        // back on the floor, which is where a draft's turns ran in the first
        // place. `None` here is the floor and not the policy file; see the
        // fallback in `run_turn` and
        // `RECORD/2026-09-21.the-drafts-floor.completed.md`.
        let plan = session.context.job(job).and_then(|job| job.plan.clone());
        session.narrowed = plan
            .and_then(|plan| plan.narrow(agency.sandbox.as_ref(), job).ok())
            .map(|sandbox| (job, Arc::new(sandbox)));
    }
    app.publish(Event::Protocol(ServerMessage::JobReopened { job }))
        .await;
}

/// Everything two kinds of model call share: the turn number, the selection,
/// and the three trace messages that explain it.
///
/// `instruction` is fused into the *current user message* when there is one —
/// never into the system block, which is the part the cache reuses. `None`
/// while a turn is already running: one at a time until sessions exist.
/// The floor, naming the draft it is holding when one is open.
///
/// `Agency::floor` is derived once from the session and knows no job, because a
/// draft's id does not exist when it is derived — but by the time a turn is
/// being built, every prompt after the first has one open in this very context.
/// Stamping it here is what gives a refusal the same denormalisation
/// [`Authority::Plan`] has always had: *the floor for draft 3* rather than *the
/// draft's floor*, with the join to `TurnStarted.job` left to nobody.
///
/// **Only when the live job is really a draft.** A job with a plan reaching the
/// floor is the widening
/// `RECORD/2026-09-21.the-drafts-floor.completed.md` §The second finding
/// closed; naming it a draft would make a recording confidently wrong about
/// which kind of job ran unapproved. It keeps the unstamped text, which is what
/// that case prints today.
///
/// One `Sandbox` clone per drafting turn, against an `Arc` clone before it: two
/// `Vec`s and a `PathBuf`, beside a model call. See
/// `RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`.
fn floor_holding(session: &Session, agency: &crate::session::Agency) -> Arc<Sandbox> {
    match session.context.live_job().filter(|job| {
        session
            .context
            .job(*job)
            .is_some_and(agent_core::job::Job::is_draft)
    }) {
        Some(draft) => Arc::new(
            agency
                .floor()
                .as_ref()
                .clone()
                .under(Authority::Draft(Some(draft))),
        ),
        None => agency.floor().clone(),
    }
}

async fn begin_turn(
    app: &Arc<App>,
    prompt: &str,
    instruction: Option<&str>,
) -> Option<(
    TurnId,
    watch::Receiver<bool>,
    CompletionRequest,
    Vec<Fragment>,
)> {
    // Taken before the session lock and held for the whole turn: a turn is
    // built, measured and sent against **one** destination, and the swap in
    // `create_session` cannot happen inside one anyway.
    let sending = app.destination().await;
    // The session's posture, for the same reason and taken the same way: what a
    // turn may read is decided by the plan when there is one and by this when
    // there is not.
    let agency = app.agency().await;
    let (turn, job, cancel_rx, selection, prompt_sent, tool_specs, reuse, code) = {
        let mut session = app.session.lock().await;
        if let Some(running) = session.current {
            drop(session);
            refuse(
                app,
                "prompt",
                Refusal::Busy,
                format!("turn {running} is running; one at a time until sessions exist"),
            )
            .await;
            return None;
        }
        let turn = session.next_turn;
        session.next_turn += 1;
        session.current = Some(turn);

        let (tx, rx) = watch::channel(false);
        session.cancel = Some(tx);
        // The sandbox the turn about to be built runs under — `session.narrowed`
        // when a plan is open, the floor otherwise. Resolved once, here, rather
        // than separately for the selector below and for the note this feeds:
        // two readings of one decision is how they drift, the same reasoning
        // the selector's own comment already gives for reading it through the
        // live job rather than the policy file.
        let sandbox = match &session.narrowed {
            Some((_, sandbox)) => sandbox.clone(),
            None => floor_holding(&session, &agency),
        };
        // What this turn is told about that authority, if this machine or this
        // session set anything. `Authority::Policy` gets no table and
        // therefore no note — see `crate::provider::AuthorityNotes`.
        let note = match sandbox.authority() {
            Authority::Draft(_) => sending.authority.draft.as_ref(),
            Authority::Plan(_) => sending.authority.plan.as_ref(),
            // Never a turn's sandbox: it holds one call a person allowed.
            Authority::Policy | Authority::Person(_) => None,
        };
        let text = match instruction {
            Some(instruction) => format!("{instruction}{prompt}"),
            None => prompt.to_string(),
        };
        // `position = "prompt"` reaches no further than this string: the note
        // rides the one thing that changes every turn anyway, so repeating it
        // costs exactly what the config says it does and nothing this
        // function does not already account for.
        let text = match note {
            Some(crate::provider::AuthorityNote {
                text: Some(words),
                position: Some(agent_core::sandbox::NotePosition::Prompt),
            }) => format!("{words}\n\n{text}"),
            _ => text,
        };
        // `position = "system"` (the default when `text` is set) rides the
        // cached prefix instead — see `Context::note_system`. Set every turn
        // regardless of whether it changed, `None` included: an authority
        // note is a fact about the turn about to render and never one carried
        // over from the one before it.
        session.context.note_system(match note {
            Some(crate::provider::AuthorityNote {
                text: Some(words),
                position: None | Some(agent_core::sandbox::NotePosition::System),
            }) => Some(words.clone()),
            _ => None,
        });
        // The fragments this turn's own text points at, read through the
        // sandbox the *live job* was granted — `session.narrowed` when a plan
        // is open, the floor otherwise. That is not a detail: the gate
        // exists so a person decides what a job may touch, and a selector
        // reading outside the approved plan would put files into the prompt
        // that the person refused.
        let code: Vec<Fragment> = match app.select_tokens {
            0 => Vec::new(),
            tokens => {
                // Re-stamped before it is scored, because the walk is a cache
                // of line numbers and this turn may be answering after an edit
                // the last turn made. Through the **agency's** sandbox: the
                // cache belongs to the session, and refreshing it through a
                // narrow plan would shrink it for every turn after. The
                // narrowing is applied at the read, below and inside `select`.
                //
                // On a blocking thread, for the reason every other filesystem
                // call in a turn is: a `std::fs` call inline in an `async`
                // block never yields, so the loop's deadline above it never
                // fires.
                let mut walked = app.walked.lock().await;
                let previous = std::mem::take(&mut *walked);
                let agency = agency.sandbox.clone();
                let rewalked = match tokio::task::spawn_blocking(move || {
                    agent_core::repo_map::rewalk_sources(agency.as_ref(), &previous)
                })
                .await
                {
                    Ok(rewalked) => rewalked,
                    Err(error) => std::panic::resume_unwind(error.into_panic()),
                };
                if rewalked.changed() {
                    eprintln!(
                        "selection — re-walked: {} changed, {} added, {} dropped",
                        rewalked.reparsed, rewalked.added, rewalked.dropped,
                    );
                }
                *walked = rewalked.walked;
                agent_core::select::select(
                    &walked,
                    sandbox.as_ref(),
                    &text,
                    tokens,
                    sending.counter.as_ref(),
                    &app.select_weights,
                )
                .specs()
                .iter()
                // A selected path the sandbox refuses is a file that is not
                // selected, not a failed turn: nobody asked for it by name.
                .filter_map(|spec| agent_core::fragment::load(sandbox.as_ref(), spec).ok())
                .collect()
            }
        };
        // Selected under the same lock that hands out the turn number, so the
        // history a turn is built from is the history at the moment it started.
        let selection =
            session
                .context
                .select(&text, &code, sending.budget, sending.counter.as_ref());
        // Measured under the same lock, so two turns cannot interleave and
        // measure themselves against each other's prompt.
        let prompt_sent = rendered(&selection.messages);
        let tool_specs = session.context.native_tools().map(str::to_string);
        let reuse = session
            .prefix
            .measure(turn, &prompt_sent, sending.counter.as_ref());
        let job = session.context.live_job();
        (
            turn,
            job,
            rx,
            selection,
            prompt_sent,
            tool_specs,
            reuse,
            code,
        )
    };

    // The user's ask, not the instruction fused in front of it: `prompt` is
    // what was asked and the trace below carries what was sent.
    app.publish(Event::Protocol(ServerMessage::TurnStarted {
        turn,
        prompt: prompt.to_string(),
        job,
    }))
    .await;
    // What it was asked *with*, by reference, straight after what was asked.
    // Every span here is `Selected`: this surface has no `--fragment`, so
    // nothing in a session the store holds was attached by hand — which is the
    // half of `RECORD/2026-09-19.fragments-by-reference.completed.md` §Two
    // things that decides how much the resume is restoring on the person's
    // behalf rather than on the selector's.
    if !code.is_empty() {
        app.publish(Event::Protocol(ServerMessage::Grounded {
            turn,
            spans: code
                .iter()
                .map(|fragment| protocol::Grounding {
                    spec: fragment.path.clone(),
                    origin: protocol::Origin::Selected,
                })
                .collect(),
        }))
        .await;
    }
    // Before the prompt it explains: this is what the turn no longer carries,
    // and a client reading in order should learn that the history was cut
    // before it is handed the prompt that was cut from.
    if let Some(evicted) = selection.eviction.clone() {
        app.publish(Event::Protocol(ServerMessage::Evicted {
            turn,
            turns: evicted.turns,
            tokens: evicted.tokens,
            counter: evicted.counter,
            policy: evicted.policy,
        }))
        .await;
    }
    app.publish(Event::Trace(TraceMessage::Prompt {
        turn,
        text: prompt_sent,
        tool_specs,
    }))
    .await;
    if let Some(reuse) = reuse {
        app.publish(Event::Trace(reuse)).await;
    }
    if let Some(pruned) = selection.pruning.clone() {
        app.publish(Event::Trace(TraceMessage::Pruned {
            turn,
            turns: pruned.turns,
            tokens: pruned.tokens,
            counter: pruned.counter,
        }))
        .await;
    }
    // Beside the prune it is the counterpart of: rule A kept spans out of this
    // render, and until format 14 the only trace of that was a smaller bucket.
    if let Some(repeated) = selection.repeating.clone() {
        app.publish(Event::Trace(TraceMessage::Repeated {
            turn,
            turns: repeated.turns,
            asking: repeated.asking,
            spans: repeated.spans,
            tokens: repeated.tokens,
            counter: repeated.counter,
        }))
        .await;
    }
    // One line per path, because each is its own finding and a render that
    // found three has three things to say. Empty on every turn of a session
    // that never edits a file it has quoted.
    for one in selection.diverged.clone() {
        app.publish(Event::Trace(TraceMessage::Diverged {
            turn,
            path: one.path,
            turns: one.turns,
            asking: one.asking,
            bodies: one.bodies,
        }))
        .await;
    }
    // The repair, beside the finding and never about the same path in the same
    // render: replacing the stale body is what leaves one body to send.
    for one in selection.superseded.clone() {
        app.publish(Event::Trace(TraceMessage::Superseded {
            turn,
            path: one.path,
            turns: one.turns,
            spans: one.spans,
            tokens: one.tokens,
            counter: one.counter,
        }))
        .await;
    }
    // Published before the call: this is what we decided to send. A turn that
    // gets cancelled has a budget too, which the old after-the-fact version
    // could not report.
    app.publish(Event::Trace(TraceMessage::Budget {
        turn,
        limit: selection.limit,
        counter: selection.counter.clone(),
        buckets: selection.buckets.clone(),
    }))
    .await;

    Some((
        turn,
        cancel_rx,
        CompletionRequest {
            // Under `native`, the tools travel here and the server's template
            // renders them; under `fenced` they are already in the system text.
            tools: agency.specs(sending.tool_calls),
            model: sending.model.clone(),
            messages: selection.messages,
            // The window we budgeted against, sent so the server serves it. The
            // same `None` the budget means by "unknown".
            context_limit: sending.budget.limit,
            temperature: app.temperature,
            seed: app.seed,
            // `Grammar` is honestly a first-attempt constraint — prose stays
            // reachable through the alternation — so it is sent on every
            // step, the same as `chat`. `Schema` is never here: it has no
            // "just answer" branch, so it is spent once, as `schema_retry`,
            // only on a reply that drifted. See
            // `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
            constraint: app.constraint.clone(),
        },
        code,
    ))
}

async fn start_turn(app: Arc<App>, prompt: String) {
    let Some((turn, cancel_rx, request, code)) = begin_turn(&app, &prompt, None).await else {
        return;
    };
    // The same destination the request was built against. See [`Destination`].
    let sending = app.destination().await;
    // And the same posture: what a turn may do is decided when its session
    // starts, so it is read once here rather than per lock.
    let agency = app.agency().await;

    // Inside an approved job, the plan it was approved with is what holds this
    // turn; outside one, **the floor** — the policy file with every grant
    // downgraded to a read. A turn is never checked against both.
    //
    // The fallback arm is what holds a turn nobody approved, and until item 22
    // it was unreachable in a served session: the gate held the first prompt,
    // so there was never an open session without an open job. The alternation
    // made it the normal case — a draft has no plan — and with it the policy
    // file, which on this repository grants `.` at `read-write` plus `cargo`
    // and `git`. So exploration could edit the tree and run the build with
    // nobody having approved anything. The floor is that one expression
    // changed, and every site that sets `narrowed = None` means it without
    // being touched. See `RECORD/2026-09-21.the-drafts-floor.completed.md`.
    let sandbox = {
        let session = app.session.lock().await;
        match (session.context.live_task(), &session.narrowed) {
            (Some(live), Some((task, sandbox))) if live == *task => sandbox.clone(),
            _ => floor_holding(&session, &agency),
        }
    };
    // What *allow once* runs a call under, when this turn may ask at all: only
    // a draft's, and only where an unsigned click is an approval this server
    // accepts. Inside a job a refusal means the plan did not grant it, and the
    // plan stays what was approved. See
    // `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
    let wider = match sandbox.authority() {
        Authority::Draft(draft) if !app.approvers.required => Some(
            agency
                .sandbox
                .as_ref()
                .clone()
                .under(Authority::Person(*draft)),
        ),
        _ => None,
    };

    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel(256);
        let forwarder = {
            let app = app.clone();
            let counter = sending.counter.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    // The tool round trips. `budget` and `prefix_reuse`
                    // describe the call that starts a turn; a turn that used a
                    // tool made more, and until these are published the panel
                    // shows their cost as chat-template overhead. Measured into
                    // the same chain as the turns, from the second call on —
                    // and a schema retry counts too, even at `step == 1`. See
                    // `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`'s
                    // "the tool-call probe's own instrument cannot see a
                    // retry".
                    if let TurnEvent::ModelCall {
                        step,
                        messages,
                        retry,
                    } = &event
                    {
                        if *step > 1 || *retry {
                            let text = rendered(messages);
                            let measured = {
                                let mut session = app.session.lock().await;
                                session.prefix.measure(turn, &text, counter.as_ref())
                            };
                            if let Some(TraceMessage::PrefixReuse {
                                shared_bytes,
                                shared_tokens,
                                prompt_tokens,
                                ..
                            }) = measured
                            {
                                app.publish(Event::Trace(TraceMessage::StepCall {
                                    turn,
                                    step: *step,
                                    text,
                                    prompt_tokens,
                                    shared_bytes,
                                    shared_tokens,
                                }))
                                .await;
                            }
                        }
                        continue;
                    }
                    // Loud on the server's own stderr, the same rule
                    // `constrain_caveat` sets for a known incompatibility at
                    // startup — this is the runtime half, discovered per
                    // compile. Not a protocol message: `from_turn_event`
                    // already answers `None` for it below, and a client has
                    // no more use for this than it does for `ModelCall`.
                    if let TurnEvent::ConstraintRefused { error } = &event {
                        eprintln!("note: a constrained retry was refused: {error}");
                        continue;
                    }
                    if let Some(message) = ServerMessage::from_turn_event(turn, event) {
                        app.publish(Event::Protocol(message)).await;
                    }
                }
            })
        };

        let watching = Watching {
            app: app.clone(),
            turn,
            events: tx.clone(),
        };
        let outcome = run_agent_turn(
            sending.backend.as_ref(),
            request,
            agency.executor(),
            sandbox.as_ref(),
            agency.limits,
            app.schema_retry.as_ref(),
            wider.as_ref().map(|wider| agent_core::agent::Ask {
                wider,
                person: &watching,
            }),
            tx,
            cancel_rx,
        )
        .await;
        // It holds a sender, and the forwarder runs until every sender is gone.
        drop(watching);
        let _ = forwarder.await;

        // Read before the turn is pushed, because pushing it is what opens a
        // draft when nothing is open — and the message that announces one has
        // to be able to tell the two apart.
        let opened_before = {
            let session = app.session.lock().await;
            session.context.current_job()
        };
        // Kept for the suggestion check below, which runs after the lock is
        // dropped: a plan block the model volunteered is a proposal, and
        // parsing it under the session lock would parse it for nothing on every
        // turn that has none.
        let answered = outcome.text.clone();

        let closed = {
            let mut session = app.session.lock().await;
            // A cancelled turn keeps its partial answer: the user saw it, so
            // the model should too. A turn that produced nothing at all is not
            // remembered — an empty assistant message is not a thing that
            // happened, and several chat templates render it as a prompt to
            // continue.
            if !outcome.text.is_empty() || !outcome.steps.is_empty() {
                session.context.push_turn_with_steps(
                    turn,
                    prompt,
                    outcome.text,
                    code,
                    outcome.steps,
                    sending.counter.as_ref(),
                );
            }
            session.current = None;
            session.cancel = None;
            session.held = None;

            // After the turn is in the history, so the close sees this turn's
            // steps — the ones that just ran the command it closes on. Before
            // the lock is dropped, so a prompt arriving between the two cannot
            // start a turn inside a task that is already folding.
            let counter = sending.counter.clone();
            let closed = session.context.close_if_met(counter.as_ref());
            if let Some((job, _)) = &closed
                && session.narrowed.as_ref().is_some_and(|(id, _)| id == job)
            {
                // The job's sandbox goes with the job, exactly as it does
                // when a person closes one.
                session.narrowed = None;
            }
            // Read under the same lock as the close, like the handler above.
            closed.map(|(job, summary)| (job, summary, session.context.replaced_by(job)))
        };

        // The draft this turn opened, if it opened one. Announced *after* the
        // turn rather than before it because a draft opens by receiving a turn:
        // there is nothing to announce until one has landed, and a turn that
        // produced nothing at all never pushed and never opened one.
        let drafted = {
            let session = app.session.lock().await;
            match session.context.current_job() {
                Some(job) if Some(job) != opened_before => session
                    .context
                    .job(job)
                    .filter(|job| job.is_draft())
                    .map(|job| (job.id, job.objective.clone())),
                _ => None,
            }
        };
        if let Some((job, objective)) = drafted {
            app.publish(Event::Protocol(ServerMessage::DraftOpened {
                job,
                turn,
                objective,
            }))
            .await;
        }

        if let Some((job, summary, replaced)) = closed {
            app.publish(Event::Protocol(ServerMessage::JobClosed {
                job,
                summary,
                by: Some(ClosedBy::ExitCode),
                replaced,
            }))
            .await;
        }

        // **The model's own door to the gate.** An ordinary draft turn whose
        // answer carries a plan block is a suggestion, and it reaches the
        // person as exactly what `request_plan` would have produced — same
        // message, same two buttons. What differs is recorded where it belongs:
        // `PlanSource::Model` on a plan nobody asked for.
        //
        // Nothing is approved by it and nothing runs under it. A suggestion is
        // the model asking to be let through the gate, which is the opposite of
        // opening it. `offer` drops it if a plan is already on the table or the
        // draft has closed since.
        if let Some(proposal) = parse_plan(&answered) {
            offer(&app, proposal, PlanSource::Model).await;
        }
    });
}

/// The plan a resumed session comes back holding, if a person never answered it.
///
/// The *last* round, not any one, and only while nothing has answered it: a
/// proposal refused or approved long ago is answered, and only a trailing
/// unanswered one is a question still waiting on a person.
///
/// It is read out of the rounds rather than out of the jobs because that is
/// where a proposal lives now — it never was a job, and under the alternation
/// it never becomes one unless it is approved.
fn pending_proposal(view: &SessionView) -> Option<Pending> {
    view.rounds
        .last()
        .filter(|round| round.answer.is_none())
        .map(|round| Pending {
            objective: round.objective.clone(),
            plan: round.plan.clone(),
            source: round.source,
        })
}

/// Strips the `.json` a static mirror needs, so both spellings reach one handler.
fn bare(id: &str) -> &str {
    id.strip_suffix(".json").unwrap_or(id)
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("no such {what}")).into_response()
}

/// The live session, then whatever the store has — the live one first because
/// it is the one a client that just connected is watching.
///
/// Its own row is left out of the stored half rather than shown twice: the
/// store is a cache of this very fold, and a listing that showed both would be
/// showing one session under two names, the older of them by however long ago
/// the last checkpoint was.
/// What this server resolved at startup.
///
/// Everything here was already decided before the listener existed and printed
/// once to stderr. The page is where a person actually is.
async fn get_settings(State(state): State<AppRouterState>) -> Response {
    let app = &state.app;
    let mut settings = app.destination().await.settings.clone();
    // The sandbox is the *session's* now, so it is read here rather than
    // carried from the destination that was resolved when the process started.
    let name = app.posture.lock().await.clone();
    let agency = app.agency().await;
    settings.sandbox = agency.describe();
    settings.base = agency.sandbox.base().display().to_string();
    settings.posture = Some(agency.posture(name));
    settings.floor = Some(Floor::of(agency.floor(), !app.approvers.required));
    Json(settings).into_response()
}

/// The postures a session may be started on, as the page offers them.
#[derive(serde::Serialize)]
struct PosturesView {
    /// Where the file is, or `None` on a machine with no state directory.
    path: Option<String>,
    /// Where the server's own policy file would run a session: the
    /// `--sandbox` it was started with, or `luu.toml` where it was started.
    /// The runtime it names first and, where that is a container, the same
    /// file on every other container runtime after it.
    own: Vec<Where>,
    /// The same for every posture, by name.
    places: std::collections::BTreeMap<String, Vec<Where>>,
    /// Whether a session may choose one here at all. False over stdio, where
    /// the process is the session.
    choosable: bool,
    /// The posture the live session named, when it named one.
    running: Option<String>,
    postures: std::collections::BTreeMap<String, crate::provider::Posture>,
}

/// What a session may be allowed to do, by name.
///
/// Read-only, and deliberately so: a posture is a policy file, and a page that
/// could write one would be a page that could widen its own sandbox. The names
/// are added to `config.toml` by whoever owns the machine — the same hand that
/// writes the policy files they point at.
/// What the workspace panels ask for: a path, relative to the sandbox base.
///
/// Absent means the base itself, which is what the tree opens on.
#[derive(Debug, Default, serde::Deserialize)]
struct WorkspaceQuery {
    #[serde(default)]
    path: String,
    /// `git-diff` only: the index against `HEAD` rather than the working tree
    /// against the index.
    #[serde(default)]
    staged: bool,
}

/// One `workspace::Error` as one HTTP answer. Here rather than as an
/// `IntoResponse` impl on the error because the mapping is this surface's
/// opinion, not the module's: `workspace` is about a filesystem and knows
/// nothing about status codes.
fn workspace_error(error: workspace::Error) -> Response {
    let status = match &error {
        // Not `FORBIDDEN`: the sandbox refusing a path is the policy working,
        // and a client that asked for something outside it asked wrongly.
        workspace::Error::Refused(_) => StatusCode::BAD_REQUEST,
        workspace::Error::Missing(_) => StatusCode::NOT_FOUND,
        workspace::Error::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, error.to_string()).into_response()
}

/// The live agency's sandbox. Read through the lock on every call rather than
/// cloned once: a session that moved to another posture moved its sandbox, and
/// a panel reading the old one would be showing a tree the running session
/// cannot reach.
async fn workspace_sandbox(state: &AppRouterState) -> Arc<agent_core::sandbox::Sandbox> {
    state.app.agency.read().await.sandbox.clone()
}

async fn get_workspace_tree(
    State(state): State<AppRouterState>,
    Query(query): Query<WorkspaceQuery>,
) -> Response {
    let sandbox = workspace_sandbox(&state).await;
    match workspace::tree(sandbox.as_ref(), &query.path).await {
        Ok(tree) => Json(tree).into_response(),
        Err(error) => workspace_error(error),
    }
}

async fn get_workspace_file(
    State(state): State<AppRouterState>,
    Query(query): Query<WorkspaceQuery>,
) -> Response {
    let sandbox = workspace_sandbox(&state).await;
    match workspace::file(sandbox.as_ref(), &query.path).await {
        Ok(file) => Json(file).into_response(),
        Err(error) => workspace_error(error),
    }
}

#[derive(serde::Deserialize)]
struct HighlightRequest {
    /// The fence's info string: `rust`, `rs`, `sh`, or empty.
    #[serde(default)]
    language: String,
    text: String,
}

/// `{ language, lines }`, the shape `/api/workspace/file` gives its lines in,
/// so a snippet in the chat is drawn by the viewer's own classes.
async fn post_highlight(Json(request): Json<HighlightRequest>) -> Response {
    let (language, lines) = crate::highlight::fenced(&request.language, &request.text);
    Json(serde_json::json!({ "language": language, "lines": lines })).into_response()
}

async fn get_workspace_git_status(State(state): State<AppRouterState>) -> Response {
    let sandbox = workspace_sandbox(&state).await;
    match workspace::git_status(sandbox.base()).await {
        Ok(map) => Json(map).into_response(),
        Err(error) => workspace_error(error),
    }
}

async fn get_workspace_git_diff(
    State(state): State<AppRouterState>,
    Query(query): Query<WorkspaceQuery>,
) -> Response {
    let sandbox = workspace_sandbox(&state).await;
    match workspace::diff(sandbox.as_ref(), &query.path, query.staged).await {
        Ok(diff) => Json(diff).into_response(),
        Err(error) => workspace_error(error),
    }
}

/// What changed under the base, as it changes: one server-sent event per
/// batch, `data:` a [`workspace::Changes`].
///
/// **Server-sent events and not the socket**, because this is not the
/// protocol: `/ws` carries the agent's messages, which stdio carries too, and
/// a file tree is something only the page has. And not polling, because the
/// page would have to ask about every open directory and the open file on a
/// timer, and still be up to a period late.
///
/// Behind the same bearer check as every other `/api/*` route, sent as a
/// header: the page reads this with `fetch`, not `EventSource`, which cannot
/// set one — so `?token=` stays the `/ws` exception it was.
async fn get_workspace_events(State(state): State<AppRouterState>) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};

    let sandbox = workspace_sandbox(&state).await;
    let watch = match workspace::Watch::start(sandbox) {
        Ok(watch) => watch,
        Err(error) => return workspace_error(error),
    };
    let stream = futures_util::stream::unfold(watch, |mut watch| async move {
        let changes = watch.next().await?;
        let event = Event::default()
            .json_data(&changes)
            .unwrap_or_else(|_| Event::default().data("{\"all\":true}"));
        Some((Ok::<_, std::convert::Infallible>(event), watch))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn get_icons_manifest(State(state): State<AppRouterState>) -> Response {
    Json(state.app.icons().manifest.clone()).into_response()
}

/// `[ui] icon-theme` as Settings shows it: what the file says, whether that
/// loaded, where imports go, and every theme this machine has. See
/// `RECORD/2026-09-28.an-icon-theme-from-settings.completed.md`.
#[derive(serde::Serialize)]
struct IconThemeView {
    /// The `config.toml` a save writes.
    path: Option<String>,
    editable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
    /// What `[ui] icon-theme` says, as written.
    named: Option<String>,
    /// The theme on screen, by name, if one is.
    loaded: Option<String>,
    /// Why what the file names is not what is on screen, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Where an import is written.
    folder: Option<String>,
    found: Vec<crate::icons::Found>,
}

/// `<state dir>/icon-themes`, beside `config.toml`.
fn icon_theme_folder() -> Option<std::path::PathBuf> {
    Some(
        crate::provider::Config::path_for_writing()?
            .parent()?
            .join("icon-themes"),
    )
}

fn icon_theme_view(state: &AppRouterState) -> IconThemeView {
    let (named, path) = match crate::provider::Config::load() {
        Ok((config, path)) => (config.ui().and_then(|ui| ui.icon_theme.clone()), path),
        Err(_) => (None, None),
    };
    let icons = state.app.icons();
    // Only asked when the file names something the screen is not showing:
    // the answer is the loader's own message, which is the useful one.
    let error = match (&named, icons.manifest.loaded) {
        (Some(named), false) => crate::icons::load(&crate::provider::expand_home(named)).err(),
        _ => None,
    };
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let folder = icon_theme_folder();
    IconThemeView {
        path: path
            .or_else(crate::provider::Config::path_for_writing)
            .map(|path| path.display().to_string()),
        editable: state.providers_editable,
        refused: (!state.providers_editable).then(|| {
            "this server is bound off loopback, and an icon theme is a path on this machine \
             and a folder written into it. Edit config.toml on the machine itself."
                .to_string()
        }),
        named: named.map(|named| named.display().to_string()),
        loaded: icons.manifest.loaded.then(|| {
            icons
                .manifest
                .name
                .clone()
                .unwrap_or_else(|| "icon theme".into())
        }),
        error,
        found: crate::icons::discover(folder.as_deref(), home.as_deref()),
        folder: folder.map(|folder| folder.display().to_string()),
    }
}

async fn get_icon_theme(State(state): State<AppRouterState>) -> Response {
    Json(blocking(move || icon_theme_view(&state)).await).into_response()
}

/// Filesystem work a handler cannot avoid, on the blocking pool instead of a
/// runtime worker.
///
/// `icon_theme_view` reads `config.toml` and a `package.json` per installed
/// extension, which is 26 ms on a quiet disk and took 98 s and 147 s on
/// 2026-09-30 — the only request in flight both times the runtime stopped
/// turning, with no socket, turn or other request served until it ended. On
/// the blocking pool the same wait is one slow request. See `RECORD/2026-10-01.a-slow-disk-is-one-slow-request.completed.md`.
///
/// A panic in `work` is resumed here, so it ends the request exactly as it
/// did when the same code ran inline.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => panic!("the blocking pool is shutting down: {error}"),
    }
}

/// `Config::load`, [`blocking`]. The file is a few hundred bytes, and it is
/// on the same disk that took 147 s to read a `package.json` — every read of
/// it a handler makes goes through here.
async fn load_config()
-> Result<(crate::provider::Config, Option<PathBuf>), crate::provider::ConfigError> {
    blocking(crate::provider::Config::load).await
}

/// `Config::path_for_writing`, [`blocking`]: it asks the disk which state
/// directory exists.
async fn config_path() -> Option<PathBuf> {
    blocking(crate::provider::Config::path_for_writing).await
}

/// The file a settings view names: the one that loaded, or where a save
/// would write one.
async fn shown_path(loaded: Option<PathBuf>) -> Option<String> {
    let path = match loaded {
        Some(path) => Some(path),
        None => config_path().await,
    };
    path.map(|path| path.display().to_string())
}

/// `Config::write`, [`blocking`].
async fn write_config(
    config: crate::provider::Config,
    path: PathBuf,
) -> Result<(), crate::provider::ConfigError> {
    blocking(move || config.write(&path)).await
}

#[derive(serde::Deserialize)]
struct IconThemeAsk {
    icon_theme: Option<String>,
}

/// Writes `[ui] icon-theme` and swaps the theme on screen, **only if it
/// loads**.
///
/// A path the page accepted and the server then ignored would be a setting
/// that looks saved, so a theme that does not load is refused with the
/// loader's message and nothing is written. `null` clears it and the tree
/// goes back to its own two shapes.
async fn put_icon_theme(
    State(state): State<AppRouterState>,
    Json(asked): Json<IconThemeAsk>,
) -> Response {
    let named = asked
        .icon_theme
        .map(|named| named.trim().to_string())
        .filter(|named| !named.is_empty());
    let applied = blocking(move || {
        apply_icon_theme(&state, named.map(std::path::PathBuf::from))
            .map(|()| icon_theme_view(&state))
    })
    .await;
    match applied {
        Ok(view) => Json(view).into_response(),
        Err(response) => *response,
    }
}

/// The write both routes end in: load, write `[ui]`, swap.
fn apply_icon_theme(
    state: &AppRouterState,
    named: Option<std::path::PathBuf>,
) -> Result<(), Box<Response>> {
    if !state.providers_editable {
        return Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                "the icon theme is read-only on a server bound off loopback",
            )
                .into_response(),
        ));
    }
    let Some(path) = crate::provider::Config::path_for_writing() else {
        return Err(Box::new(
            (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to write config.toml. \
             Run luu once on a terminal, or set LUU_HOME.",
        )
            .into_response(),
        ));
    };
    let theme = match &named {
        None => crate::icons::Theme::default(),
        Some(named) => crate::icons::load(&crate::provider::expand_home(named))
            .map_err(|error| Box::new((StatusCode::UNPROCESSABLE_ENTITY, error).into_response()))?,
    };
    // Built from the file on disk, for the reason every writer here is: the
    // page writes one table and must not delete the rest.
    let current = crate::provider::Config::load()
        .map(|(config, _)| config)
        .unwrap_or_default();
    // The rest of `[ui]` as it was: this route owns one key of it.
    let mut ui = current.ui().cloned().unwrap_or_default();
    ui.icon_theme = named;
    current.with_ui(ui).write(&path).map_err(|error| {
        Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
    })?;
    state.app.swap_icons(theme);
    Ok(())
}

/// A folder picked in the browser, written under `<state dir>/icon-themes/`
/// and then chosen.
///
/// Multipart, one `file` part per file, each part's filename being the path
/// the browser's directory picker gave it (`webkitRelativePath`). Every path is
/// re-checked by [`crate::icons::import`], which also refuses a folder that
/// does not load, before it replaces anything.
async fn import_icon_theme(
    State(state): State<AppRouterState>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    if !state.providers_editable {
        return (
            StatusCode::FORBIDDEN,
            "the icon theme is read-only on a server bound off loopback",
        )
            .into_response();
    }
    let Some(folder) = blocking(icon_theme_folder).await else {
        return (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to import into",
        )
            .into_response();
    };
    let mut files = Vec::new();
    let mut total = 0usize;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        };
        let Some(name) = field.file_name().map(str::to_string) else {
            continue;
        };
        let bytes = match field.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        };
        total += bytes.len();
        if total > crate::icons::IMPORT_MAX_BYTES || files.len() >= crate::icons::IMPORT_MAX_FILES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "the folder is too big to be an icon theme",
            )
                .into_response();
        }
        files.push((name, bytes.to_vec()));
    }
    let imported =
        match tokio::task::spawn_blocking(move || crate::icons::import(&folder, files)).await {
            Ok(Ok(imported)) => imported,
            Ok(Err(error)) => return (StatusCode::UNPROCESSABLE_ENTITY, error).into_response(),
            Err(error) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
            }
        };
    // Written as `~/…` when it can be, so the file reads the same on another
    // machine with the same layout.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let named = match home
        .as_deref()
        .and_then(|home| imported.strip_prefix(home).ok())
    {
        Some(rest) => std::path::Path::new("~").join(rest),
        None => imported,
    };
    let applied =
        blocking(move || apply_icon_theme(&state, Some(named)).map(|()| icon_theme_view(&state)))
            .await;
    match applied {
        Ok(view) => Json(view).into_response(),
        Err(response) => *response,
    }
}

/// One icon's bytes.
///
/// The id is looked up in the table the theme was read into; an id that is not
/// in it is a 404 and never a filesystem access. That is the whole of the path
/// safety here, and it is why there is no path to validate.
async fn get_icon(State(state): State<AppRouterState>, Path(id): Path<String>) -> Response {
    let icons = state.app.icons();
    let Some(path) = icons.path(&id) else {
        return (StatusCode::NOT_FOUND, "no such icon").into_response();
    };
    let content_type = crate::icons::content_type(path);
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type),
                // The page asks with the theme's `revision` in the URL, so a
                // swapped theme is a different URL and the browser may keep
                // these.
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not read the icon: {error}"),
        )
            .into_response(),
    }
}

/// A posture as it names itself, and — where that is a container — the same
/// file on every other container runtime: one image, one set of in-image
/// paths, one argv but for its first word, which is what makes them
/// substitutable. A host posture has no variants, because its sandbox names
/// paths on this machine and an image's `[[worker.paths]]` are not here. See
/// `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
fn variants(describe: &DescribeFactory, policy: Option<PathBuf>) -> Vec<Where> {
    let declared = describe(policy.clone(), None);
    let contained = declared
        .runtime
        .parse::<agent_core::worker::Runtime>()
        .is_ok_and(|runtime| runtime.is_contained());
    let mut all = vec![declared.clone()];
    if contained {
        all.extend(
            agent_core::worker::Runtime::CONTAINED
                .into_iter()
                .filter(|runtime| runtime.as_str() != declared.runtime)
                .map(|runtime| describe(policy.clone(), Some(runtime))),
        );
    }
    all
}

async fn get_postures(State(state): State<AppRouterState>) -> Response {
    let app = &state.app;
    // Files, read on the blocking pool for `blocking`'s reason.
    let (own, places) = match app.describe_for.clone() {
        None => (Vec::new(), Default::default()),
        Some(describe) => {
            let postures = app.postures();
            blocking(move || {
                let variants = |policy: Option<PathBuf>| variants(&describe, policy);
                let places = postures
                    .iter()
                    .map(|(name, posture)| (name.clone(), variants(Some(posture.policy.clone()))))
                    .collect();
                (variants(None), places)
            })
            .await
        }
    };
    Json(PosturesView {
        path: app.postures_path.clone(),
        own,
        places,
        choosable: app.agency_for.is_some(),
        running: app.posture.lock().await.clone(),
        postures: app.postures(),
    })
    .into_response()
}

// ---- runtimes ----------------------------------------------------------------
//
// Settings → Runtimes. Every write is held to [`engines_allowed`] — loopback by
// the bound address and by `Host` — because a posture is what a session may do
// and an image build is a process this machine runs. See `crate::runtimes` and
// `RECORD/2026-10-01.runtimes-in-settings.completed.md`.

#[derive(serde::Serialize)]
struct PostureRow {
    policy: String,
    place: Option<Where>,
}

#[derive(serde::Serialize)]
struct PolicyRow {
    path: String,
    place: Option<Where>,
}

#[derive(serde::Serialize)]
struct RuntimesView {
    /// Where `config.toml` is, or would be written.
    path: Option<String>,
    editable: bool,
    refused: Option<String>,
    /// Where an image is built from, and where a policy file is looked for.
    base: String,
    runtimes: Vec<crate::runtimes::RuntimeView>,
    tasks: Vec<crate::runtimes::TaskView>,
    postures: std::collections::BTreeMap<String, PostureRow>,
    /// The policy files a new posture may name.
    policies: Vec<PolicyRow>,
    /// The posture the live session is on, which may not be removed.
    running: Option<String>,
    /// `[terminal] host`.
    host_shell: &'static str,
}

/// The images a runtime is asked about and may build: the ones a policy file
/// or a posture names. Never one the page types — an image is a thing
/// somebody wrote into a file here, for the reason a posture is never a path.
fn images_of<'a>(places: impl Iterator<Item = &'a Option<Where>>) -> Vec<String> {
    let mut images: Vec<String> = places
        .flatten()
        .filter_map(|place| place.image.clone())
        .collect();
    images.sort();
    images.dedup();
    images
}

async fn runtimes_answer(state: &AppRouterState, headers: &axum::http::HeaderMap) -> Response {
    let app = &state.app;
    let base = app.agency().await.sandbox.base().to_path_buf();
    let postures = app.postures();
    let describe = app.describe_for.clone();
    let (postures, policies) = {
        let base = base.clone();
        blocking(move || {
            let place = |policy: Option<PathBuf>| describe.as_ref().map(|d| d(policy, None));
            let postures: std::collections::BTreeMap<_, _> = postures
                .into_iter()
                .map(|(name, posture)| {
                    (
                        name,
                        PostureRow {
                            policy: posture.policy.display().to_string(),
                            place: place(Some(posture.policy.clone())),
                        },
                    )
                })
                .collect();
            let policies: Vec<_> = crate::runtimes::policy_files(&base)
                .into_iter()
                .map(|file| PolicyRow {
                    place: place(Some(base.join(&file.path))),
                    path: file.path,
                })
                .collect();
            (postures, policies)
        })
        .await
    };
    let images = images_of(
        postures
            .values()
            .map(|row| &row.place)
            .chain(policies.iter().map(|row| &row.place)),
    );
    let runtimes = crate::runtimes::probe_all(&images).await;
    let refused = engines_allowed(state, headers).err().map(|_| {
        "Runtimes are configured from a browser on this machine: a posture is what a session \
         may do, and a build is a process this machine runs."
            .to_string()
    });
    let host_shell = load_config()
        .await
        .map(|(config, _)| config.terminal().host)
        .unwrap_or_default()
        .as_str();
    Json(RuntimesView {
        path: config_path().await.map(|path| path.display().to_string()),
        editable: refused.is_none(),
        refused,
        base: base.display().to_string(),
        runtimes,
        tasks: app.tasks.list(),
        postures,
        policies,
        running: app.posture.lock().await.clone(),
        host_shell,
    })
    .into_response()
}

async fn get_runtimes(
    headers: axum::http::HeaderMap,
    State(state): State<AppRouterState>,
) -> Response {
    runtimes_answer(&state, &headers).await
}

/// The tasks alone: what the section polls while one runs, because the
/// probes start a process each and this starts none.
async fn get_tasks(State(state): State<AppRouterState>) -> Response {
    Json(state.app.tasks.list()).into_response()
}

/// A container runtime named in a route, and nothing else.
fn contained(runtime: &str) -> Result<agent_core::worker::Runtime, Box<Response>> {
    match runtime.parse::<agent_core::worker::Runtime>() {
        Ok(runtime) if runtime.is_contained() => Ok(runtime),
        Ok(runtime) => Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                format!("`{runtime}` is not a container runtime"),
            )
                .into_response(),
        )),
        Err(error) => Err(Box::new((StatusCode::BAD_REQUEST, error).into_response())),
    }
}

/// Installs a runtime, where `crate::runtimes::install_steps` has a line for
/// it: Homebrew on macOS, for podman and colima, with the person's own
/// authority. A JSON body is required for `start_engine`'s reason.
async fn install_runtime(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Path(runtime): Path<String>,
    Json(_): Json<serde_json::Value>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    let runtime = match contained(&runtime) {
        Ok(runtime) => runtime,
        Err(refused) => return *refused,
    };
    let base = state.app.agency().await.sandbox.base().to_path_buf();
    match state.app.tasks.install(runtime, &base) {
        Ok(()) => Json(state.app.tasks.list()).into_response(),
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}

/// Starts a runtime that is installed and not answering: Docker Desktop, a
/// podman machine, colima, Apple's system service.
async fn start_runtime(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Path(runtime): Path<String>,
    Json(_): Json<serde_json::Value>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    let runtime = match contained(&runtime) {
        Ok(runtime) => runtime,
        Err(refused) => return *refused,
    };
    let base = state.app.agency().await.sandbox.base().to_path_buf();
    match state.app.tasks.wake(runtime, &base) {
        Ok(()) => Json(state.app.tasks.list()).into_response(),
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct BuildAsk {
    image: String,
}

async fn build_image(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Path(runtime): Path<String>,
    Json(asked): Json<BuildAsk>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    let runtime = match contained(&runtime) {
        Ok(runtime) => runtime,
        Err(refused) => return *refused,
    };
    let app = &state.app;
    let base = app.agency().await.sandbox.base().to_path_buf();
    // Only an image some file here names.
    let named = {
        let base = base.clone();
        let postures = app.postures();
        let describe = app.describe_for.clone();
        blocking(move || {
            let place = |policy: PathBuf| describe.as_ref().map(|d| d(Some(policy), None));
            let places: Vec<Option<Where>> = postures
                .values()
                .map(|posture| place(posture.policy.clone()))
                .chain(
                    crate::runtimes::policy_files(&base)
                        .into_iter()
                        .map(|file| place(base.join(file.path))),
                )
                .collect();
            images_of(places.iter())
        })
        .await
    };
    if !named.contains(&asked.image) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "no policy file here names `{}` as its [worker] image, so it is not one this page builds",
                asked.image
            ),
        )
            .into_response();
    }
    match app.tasks.build(runtime, &asked.image, &base) {
        Ok(()) => Json(app.tasks.list()).into_response(),
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PosturesAsk {
    postures: std::collections::BTreeMap<String, crate::provider::Posture>,
}

/// Replaces `[posture.*]`, and the postures this server offers with it.
///
/// A new posture names a policy file **out of the list** [`policy_files`]
/// makes, never one the page types; a posture the file already had keeps
/// whatever it names, whoever wrote it. The live session's posture may not be
/// removed: it would be running under a name the file no longer has.
///
/// [`policy_files`]: crate::runtimes::policy_files
async fn put_postures(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Json(asked): Json<PosturesAsk>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    let app = &state.app;
    let Some(path) = config_path().await else {
        return (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to write config.toml",
        )
            .into_response();
    };
    let current = match load_config().await {
        Ok((config, _)) => config,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    let base = app.agency().await.sandbox.base().to_path_buf();
    let offered: Vec<String> = {
        let base = base.clone();
        blocking(move || {
            crate::runtimes::policy_files(&base)
                .into_iter()
                .map(|file| file.path)
                .collect()
        })
        .await
    };
    for (name, posture) in &asked.postures {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("`{name}` is not a posture name: letters, digits, `-` and `_`"),
            )
                .into_response();
        }
        let kept = current.postures().get(name) == Some(posture);
        let listed = offered
            .iter()
            .any(|file| std::path::Path::new(file) == posture.policy);
        if !kept && !listed {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "{} is not a policy file in {}: from this page a posture names one of those, \
                     and any other path is written in config.toml by hand",
                    posture.policy.display(),
                    base.display()
                ),
            )
                .into_response();
        }
    }
    if let Some(running) = app.posture.lock().await.clone()
        && !asked.postures.contains_key(&running)
    {
        return (
            StatusCode::CONFLICT,
            format!(
                "the live session is on `{running}`: move it to another posture first, from the \
                 terminal's picker"
            ),
        )
            .into_response();
    }
    let postures = asked.postures.clone();
    if let Err(error) = write_config(current.with_postures(asked.postures), path).await {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }
    *app.postures
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = postures;
    runtimes_answer(&state, &headers).await
}

#[derive(serde::Deserialize)]
struct TerminalAsk {
    host: crate::provider::HostShell,
}

/// Writes `[terminal] host`. Read again by every request for a shell, so it
/// takes effect at once.
async fn put_terminal(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Json(asked): Json<TerminalAsk>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    let Some(path) = config_path().await else {
        return (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to write config.toml",
        )
            .into_response();
    };
    let current = match load_config().await {
        Ok((config, _)) => config,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    let terminal = crate::provider::Terminal { host: asked.host };
    if let Err(error) = write_config(current.with_terminal(terminal), path).await {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }
    runtimes_answer(&state, &headers).await
}

// ---- hosts -----------------------------------------------------------------
//
// See `crate::hosts` and
// `RECORD/2026-10-02.a-project-is-a-host-a-folder-and-a-session.completed.md`.

/// Whether this request may reach a host, or write the list of them.
///
/// [`engines_allowed`]'s rule, and for its reason: a host is reached with its
/// token, which carries its approvals, its engines and its terminal. Off
/// loopback, this server's token would hand all of that to whoever holds it.
/// And **an `Origin`, when one is sent, is this server**: a page on another
/// site cannot read a cross-origin answer, but it can send a `POST`, and
/// through here that would be a `POST` to the host.
fn hosts_allowed(
    state: &AppRouterState,
    headers: &axum::http::HeaderMap,
) -> Result<(), Box<Response>> {
    let refuse = |why: String| Box::new((StatusCode::FORBIDDEN, why).into_response());
    if !state.providers_editable {
        return Err(refuse(
            "this server is bound off loopback, and a host is reached with its own token. \
             Reach it from a browser on this machine."
                .to_string(),
        ));
    }
    let text = |name| {
        headers
            .get(name)
            .and_then(|value: &axum::http::HeaderValue| value.to_str().ok())
            .unwrap_or("")
    };
    let host = text(header::HOST);
    if !host_is_loopback(host) {
        return Err(refuse(format!(
            "Host `{host}` is not this machine, and only this machine reaches a host"
        )));
    }
    let origin = text(header::ORIGIN);
    let from = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    if !origin.is_empty() && from != Some(host) {
        return Err(refuse(format!(
            "Origin `{origin}` is not this server (`{host}`)"
        )));
    }
    Ok(())
}

/// One `[host.<name>]`, or the answer that says it is not there.
async fn host_named(name: &str) -> Result<crate::provider::Host, Box<Response>> {
    let config = load_config()
        .await
        .map(|(config, _)| config)
        .map_err(|error| {
            Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        })?;
    config.hosts().get(name).cloned().ok_or_else(|| {
        Box::new(
            (
                StatusCode::NOT_FOUND,
                format!("there is no [host.{name}] in config.toml"),
            )
                .into_response(),
        )
    })
}

/// What follows `/h/<name>` in the request, query included.
fn host_rest(uri: &Uri, name: &str) -> String {
    let path = uri.path();
    let rest = path
        .strip_prefix("/h/")
        .and_then(|tail| tail.strip_prefix(name))
        .unwrap_or(path);
    match uri.query() {
        Some(query) => format!("{rest}?{query}"),
        None => rest.to_string(),
    }
}

#[derive(serde::Serialize)]
struct HostsView {
    hosts: std::collections::BTreeMap<String, crate::provider::Host>,
    /// Whether this server may reach or write them; see [`hosts_allowed`].
    editable: bool,
    /// The file they are in, or would be written to.
    path: Option<String>,
}

async fn hosts_view(state: &AppRouterState) -> Result<HostsView, crate::provider::ConfigError> {
    let (config, loaded) = load_config().await?;
    Ok(HostsView {
        hosts: config.hosts().clone(),
        editable: state.providers_editable,
        path: shown_path(loaded).await,
    })
}

async fn get_hosts(State(state): State<AppRouterState>) -> Response {
    match hosts_view(&state).await {
        Ok(view) => Json(view).into_response(),
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct HostsEdit {
    hosts: std::collections::BTreeMap<String, crate::provider::Host>,
}

/// Replaces every `[host.*]`, and nothing else in the file.
async fn put_hosts(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Json(edit): Json<HostsEdit>,
) -> Response {
    if let Err(refused) = hosts_allowed(&state, &headers) {
        return *refused;
    }
    let Some(path) = config_path().await else {
        return (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to write config.toml. \
             Run luu once on a terminal, or set LUU_HOME.",
        )
            .into_response();
    };
    let current = load_config()
        .await
        .map(|(config, _)| config)
        .unwrap_or_default();
    if let Err(error) = write_config(current.with_hosts(edit.hosts), path).await {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }
    get_hosts(State(state)).await
}

/// A request the page made of a host's API, forwarded.
async fn host_api(
    State(state): State<AppRouterState>,
    Path((name, _)): Path<(String, String)>,
    request: Request,
) -> Response {
    if let Err(refused) = hosts_allowed(&state, request.headers()) {
        return *refused;
    }
    let host = match host_named(&name).await {
        Ok(host) => host,
        Err(refused) => return *refused,
    };
    let rest = host_rest(request.uri(), &name);
    let (parts, body) = request.into_parts();
    crate::hosts::forward(&name, &host, parts.method, &rest, &parts.headers, body).await
}

/// A socket the page opened on a host, forwarded. **Same-origin only**, as
/// the terminal is: a page on another site can open a socket anywhere, and
/// this one carries the host's authority.
async fn host_socket(
    State(state): State<AppRouterState>,
    Path(params): Path<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(refused) = hosts_allowed(&state, &headers) {
        return *refused;
    }
    if headers.get(header::ORIGIN).is_none() {
        return (
            StatusCode::FORBIDDEN,
            "a socket to a host needs an Origin, and only this server's page has one",
        )
            .into_response();
    }
    let name = params.get("name").cloned().unwrap_or_default();
    let host = match host_named(&name).await {
        Ok(host) => host,
        Err(refused) => return *refused,
    };
    let upstream = match crate::hosts::connect(&name, &host, &host_rest(&uri, &name)).await {
        Ok(upstream) => upstream,
        Err(refused) => return *refused,
    };
    ws.on_upgrade(move |socket| async move {
        tracing::info!(target: "luu::ws", host = name, "forwarding");
        crate::hosts::pipe(socket, upstream).await;
        tracing::info!(target: "luu::ws", host = name, "closed");
    })
}

/// `/h/<name>` without its slash: every relative URL on the page would
/// resolve one level too high.
async fn host_page_bare(Path(name): Path<String>) -> Response {
    axum::response::Redirect::permanent(&format!("/h/{name}/")).into_response()
}

/// This server's own page and its assets, under a host's prefix.
async fn host_page(Path((_, path)): Path<(String, String)>) -> Response {
    if let Some(rest) = path.strip_prefix("vendor/monaco/") {
        return installed_asset(monaco_root(), "monaco", rest).await;
    }
    if let Some(rest) = path.strip_prefix("vendor/xterm/") {
        return installed_asset(xterm_root(), "xterm.js", rest).await;
    }
    serve_asset(&path).await
}

// ---- engines ---------------------------------------------------------------
//
// See `crate::engines` and `RECORD/2026-09-28.a-model-server-luu-starts.completed.md`.

/// Whether this request may start, stop, download or configure an engine.
///
/// Loopback by the bound address, like every write from Settings — **and by
/// `Host`**, which nothing else here checks. A page on any domain that rebinds
/// its DNS to 127.0.0.1 is same-origin with this server; against
/// `PUT /api/providers` that costs a config file, against an engine it would be
/// an execution. A browser always sends the name it dialled, so the page on
/// this machine passes and the rebinding one does not.
fn engines_allowed(
    state: &AppRouterState,
    headers: &axum::http::HeaderMap,
) -> Result<(), Box<Response>> {
    let refuse = |why: String| Box::new((StatusCode::FORBIDDEN, why).into_response());
    if !state.providers_editable {
        return Err(refuse(
            "this server is bound off loopback, and an engine is a process started on this \
             machine. Start it from a browser on the machine itself."
                .to_string(),
        ));
    }
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .unwrap_or("");
    match host_is_loopback(host) {
        true => Ok(()),
        false => Err(refuse(format!(
            "Host `{host}` is not this machine, and only this machine starts an engine"
        ))),
    }
}

fn host_is_loopback(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[derive(serde::Serialize)]
struct EngineView {
    #[serde(flatten)]
    config: crate::provider::Engine,
    status: crate::engines::Status,
    /// The binary it would run, or why there is none.
    resolved: Option<String>,
    problem: Option<String>,
    /// Where a profile for it sends.
    url: String,
    /// The profiles that name it.
    providers: Vec<String>,
}

#[derive(serde::Serialize)]
struct EnginesView {
    path: Option<String>,
    editable: bool,
    refused: Option<String>,
    engines: std::collections::BTreeMap<String, EngineView>,
    /// Every copy of each kind's binary on this machine.
    found: std::collections::BTreeMap<&'static str, Vec<String>>,
    /// The version luu downloads when a table names none.
    pinned: std::collections::BTreeMap<&'static str, &'static str>,
    /// luu's own copies already downloaded, per kind, by directory name.
    managed: std::collections::BTreeMap<&'static str, Vec<String>>,
    /// Where they go.
    root: Option<String>,
    installs: Vec<crate::engines::Install>,
}

const KINDS: [crate::provider::EngineKind; 3] = [
    crate::provider::EngineKind::Llama,
    crate::provider::EngineKind::Mlx,
    crate::provider::EngineKind::Ollama,
];

async fn engines_view(state: &AppRouterState) -> Result<EnginesView, crate::provider::ConfigError> {
    let (config, path) = load_config().await?;
    let supervisor = &state.app.engines;
    // Everything this view asks the disk, in one trip to the pool: which
    // binary each engine resolves to, what this machine has installed, and
    // luu's own copies.
    let listed = config.engines().clone();
    let (mut resolved, found, root, managed) = blocking(move || {
        let resolved: std::collections::BTreeMap<String, Result<PathBuf, String>> = listed
            .iter()
            .map(|(name, engine)| (name.clone(), crate::engines::resolve_binary(engine)))
            .collect();
        let found: std::collections::BTreeMap<&'static str, Vec<String>> = KINDS
            .iter()
            .map(|kind| {
                let found = crate::engines::discover(*kind)
                    .into_iter()
                    .map(|p| p.display().to_string())
                    .collect();
                (kind.as_str(), found)
            })
            .collect();
        let root = crate::engines::root();
        let managed: std::collections::BTreeMap<&'static str, Vec<String>> = KINDS
            .iter()
            .map(|kind| {
                let mut copies: Vec<String> = root
                    .as_ref()
                    .and_then(|root| std::fs::read_dir(root.join(kind.as_str())).ok())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|name| !name.starts_with('.'))
                    .collect();
                copies.sort();
                (kind.as_str(), copies)
            })
            .collect();
        (resolved, found, root, managed)
    })
    .await;
    let mut engines = std::collections::BTreeMap::new();
    for (name, engine) in config.engines() {
        let (resolved, problem) = match resolved.remove(name) {
            Some(Ok(path)) => (Some(path.display().to_string()), None),
            Some(Err(problem)) => (None, Some(problem)),
            None => (None, None),
        };
        engines.insert(
            name.clone(),
            EngineView {
                status: supervisor.status(name, engine).await,
                url: engine
                    .kind
                    .url(engine.port.unwrap_or(engine.kind.default_port())),
                providers: config
                    .profiles()
                    .iter()
                    .filter(|(_, p)| p.engine.as_deref() == Some(name))
                    .map(|(n, _)| n.clone())
                    .collect(),
                config: engine.clone(),
                resolved,
                problem,
            },
        );
    }
    Ok(EnginesView {
        path: shown_path(path).await,
        editable: state.providers_editable,
        refused: (!state.providers_editable).then(|| {
            "this server is bound off loopback, and an engine is a process started on this \
             machine. Edit config.toml on the machine itself."
                .to_string()
        }),
        engines,
        found,
        pinned: KINDS.iter().map(|k| (k.as_str(), k.pinned())).collect(),
        managed,
        root: root.map(|root| root.display().to_string()),
        installs: supervisor.installs(),
    })
}

async fn engines_answer(state: &AppRouterState) -> Response {
    match engines_view(state).await {
        Ok(view) => Json(view).into_response(),
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    }
}

/// Every model on this machine, from ollama, llama.cpp, Hugging Face and luu's
/// own directory. See `crate::models`.
async fn get_models() -> Response {
    #[derive(serde::Serialize)]
    struct Entry {
        #[serde(flatten)]
        model: crate::models::LocalModel,
        /// Engine kind → why it cannot load this, for each one that cannot:
        /// the rule stays in `models::LocalModel::runs_on`, not in the page.
        cannot: std::collections::BTreeMap<&'static str, String>,
    }
    #[derive(serde::Serialize)]
    struct Catalog {
        models: Vec<Entry>,
        roots: std::collections::BTreeMap<&'static str, Option<String>>,
    }
    let models = tokio::task::spawn_blocking(crate::models::catalog)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|model| Entry {
            cannot: KINDS
                .iter()
                .filter_map(|kind| Some((kind.as_str(), model.runs_on(*kind).err()?)))
                .collect(),
            model,
        })
        .collect();
    let roots = [
        crate::models::Source::Ollama,
        crate::models::Source::LlamaCpp,
        crate::models::Source::HuggingFace,
        crate::models::Source::Luu,
    ]
    .into_iter()
    .map(|s| {
        (
            s.as_str(),
            crate::models::root(s).map(|p| p.display().to_string()),
        )
    })
    .collect();
    Json(Catalog { models, roots }).into_response()
}

/// The live session's engine, and whether it has loaded the model the session
/// is on — what the chat head polls after a switch, since the switch itself no
/// longer waits for it. `null` when the destination names no engine.
#[derive(serde::Serialize)]
struct LiveEngine {
    name: String,
    #[serde(flatten)]
    readiness: Option<crate::engines::ReadinessView>,
}

async fn live_engine(State(state): State<AppRouterState>) -> Response {
    let sending = state.app.destination().await;
    Json(sending.engine.as_ref().map(|(name, _)| LiveEngine {
        name: name.clone(),
        readiness: state.app.engines.readiness(name),
    }))
    .into_response()
}

async fn get_engines(State(state): State<AppRouterState>) -> Response {
    engines_answer(&state).await
}

#[derive(serde::Deserialize)]
struct EnginesAsk {
    engines: std::collections::BTreeMap<String, crate::provider::Engine>,
}

/// Replaces `[engine.*]`, and only if the file still loads — a profile whose
/// `engine` would name a table this removes is refused by the loader.
async fn put_engines(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Json(asked): Json<EnginesAsk>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    let Some(path) = config_path().await else {
        return (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to write config.toml",
        )
            .into_response();
    };
    let current = match load_config().await {
        Ok((config, _)) => config,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    for (name, engine) in &asked.engines {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("`{name}` is not an engine name: letters, digits, `-` and `_`"),
            )
                .into_response();
        }
        // The page may name only the kind's own binary; a path the file
        // already had is kept as it was, whoever wrote it.
        if let Some(binary) = &engine.binary {
            let kept = current.engines().get(name).and_then(|e| e.binary.as_ref()) == Some(binary);
            if !kept && !crate::engines::accepts_from_page(engine.kind, binary) {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "{binary} is not a {}: from this page an engine runs only its own \
                         binary. Any other path is written in config.toml by hand.",
                        engine.kind.binary_name()
                    ),
                )
                    .into_response();
            }
        }
    }
    let removed: Vec<String> = current
        .engines()
        .keys()
        .filter(|name| !asked.engines.contains_key(*name))
        .cloned()
        .collect();
    if let Err(error) = write_config(current.with_engines(asked.engines), path).await {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }
    for name in removed {
        state.app.engines.stop(&name).await;
    }
    engines_answer(&state).await
}

/// Starts one without waiting for it: the page polls, and shows its output
/// while it loads. A port something else already answers on is refused,
/// because a second server there would fail to bind and say so less clearly.
async fn start_engine(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
    // Required, and empty: a JSON body is what a cross-site form cannot send
    // without a preflight this server never answers.
    Json(_): Json<serde_json::Value>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    let engine = match load_config().await {
        Ok((config, _)) => config.engines().get(&name).cloned(),
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    let Some(engine) = engine else {
        return (
            StatusCode::NOT_FOUND,
            format!("there is no [engine.{name}]"),
        )
            .into_response();
    };
    let status = state.app.engines.status(&name, &engine).await;
    if status.answering && status.state != "running" {
        let port = engine.port.unwrap_or(engine.kind.default_port());
        return (
            StatusCode::CONFLICT,
            format!("something already answers on port {port}, and it is not one luu started"),
        )
            .into_response();
    }
    if let Err(error) = state.app.engines.start(&name, &engine).await {
        return (StatusCode::UNPROCESSABLE_ENTITY, error).into_response();
    }
    engines_answer(&state).await
}

async fn stop_engine(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
    Json(_): Json<serde_json::Value>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    state.app.engines.stop(&name).await;
    engines_answer(&state).await
}

#[derive(serde::Deserialize)]
struct InstallAsk {
    kind: crate::provider::EngineKind,
    version: Option<String>,
    variant: Option<String>,
}

async fn install_engine(
    State(state): State<AppRouterState>,
    headers: axum::http::HeaderMap,
    Json(asked): Json<InstallAsk>,
) -> Response {
    if let Err(refused) = engines_allowed(&state, &headers) {
        return *refused;
    }
    let blank = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if let Err(error) =
        state
            .app
            .engines
            .install(asked.kind, blank(asked.version), blank(asked.variant))
    {
        return (StatusCode::UNPROCESSABLE_ENTITY, error).into_response();
    }
    engines_answer(&state).await
}

/// The providers file, as the editor sees it.
#[derive(serde::Serialize)]
struct ProvidersView {
    /// Where the file is, or would be written. `None` is a machine that has
    /// chosen no state directory, where there is nowhere to put one.
    path: Option<String>,
    /// Whether this surface may write. False renders the section disabled
    /// rather than hiding it: a control that is absent looks like a feature
    /// that does not exist.
    editable: bool,
    /// Why not, when it is not.
    refused: Option<String>,
    default: Option<String>,
    providers: std::collections::BTreeMap<String, crate::provider::Profile>,
    /// The profile *this* server is running, which the file's default may no
    /// longer be: the file can be edited while a server that resolved hours ago
    /// keeps sending where it was pointed.
    running: Option<String>,
    /// Whether the live session follows the file's default. See
    /// [`Settings::follows_default`].
    follows: bool,
    /// Why the live session is not on the file's default when it follows it:
    /// a turn in flight, or turns already sent to the old one.
    #[serde(skip_serializing_if = "Option::is_none")]
    waiting: Option<String>,
}

async fn providers_view(
    state: &AppRouterState,
) -> Result<ProvidersView, crate::provider::ConfigError> {
    let (config, path) = load_config().await?;
    let (running, follows) = {
        let destination = state.app.destination().await;
        (
            destination.settings.profile.clone(),
            destination.settings.follows_default,
        )
    };
    Ok(ProvidersView {
        path: shown_path(path).await,
        editable: state.providers_editable,
        refused: match state.providers_editable {
            true => None,
            false => Some(
                "this server is bound off loopback. A provider outlives the session and the \
                 gate, and the bearer token says who may reach the port rather than who may \
                 decide where this machine sends. Edit config.toml on the machine itself."
                    .to_string(),
            ),
        },
        default: config.default_name().map(str::to_string),
        providers: config.profiles().clone(),
        running: running.clone(),
        follows,
        waiting: match follows && running.as_deref() != config.default_name() {
            false => None,
            true => {
                let session = state.app.session.lock().await;
                Some(
                    match session.current.is_some() || session.pending.is_some() {
                        true => "a turn is running; it moves to the default when you start a new \
                             session"
                            .to_string(),
                        false => format!(
                            "the live session has {} turn(s) on the previous destination, and moving \
                         a conversation to another model is a choice rather than a side effect",
                            session.next_turn.saturating_sub(1)
                        ),
                    },
                )
            }
        },
    })
}

async fn get_providers(State(state): State<AppRouterState>) -> Response {
    match providers_view(&state).await {
        Ok(view) => Json(view).into_response(),
        // A file that does not load is the one a person most needs to see, so
        // the message goes to the page rather than only to the terminal that
        // started this.
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    }
}

/// What one provider will answer for, and which model a session there should
/// start on.
///
/// **A provider that is not running is not an error here.** On a laptop it is
/// the ordinary state — Ollama is not started, `llama-server` is not up — and a
/// 502 in the page would read as a broken page rather than as a machine that is
/// simply off. So the list comes back empty with `reason` set, and the field
/// stays typable: a model that has not been pulled yet is a legitimate thing to
/// write down.
#[derive(serde::Serialize)]
struct ProviderModels {
    models: Vec<String>,
    /// Why the list is empty, when something went wrong producing it.
    reason: Option<String>,
    /// What a session here should start on: the model last used with this
    /// profile, else the profile's own, else the first the provider listed.
    suggested: Option<String>,
    /// Where that suggestion came from, so the page can say.
    suggested_from: Option<&'static str>,
    /// What to show for each model: `qwen2.5-coder:7b (ollama)`. Where a name
    /// is absent the page shows the name itself.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    labels: std::collections::BTreeMap<String, String>,
    /// Models on this machine the provider's engine cannot load, with why:
    /// shown greyed out rather than hidden, so a model that is on the disk
    /// does not look missing. See `models::LocalModel::runs_on`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unavailable: Vec<Unavailable>,
}

#[derive(serde::Serialize)]
struct Unavailable {
    model: String,
    label: String,
    reason: String,
}

/// The models one profile offers. Reaches the destination — see the record.
async fn get_provider_models(
    State(state): State<AppRouterState>,
    Path(name): Path<String>,
) -> Response {
    let name = bare(&name);
    let (config, path) = match load_config().await {
        Ok(loaded) => loaded,
        Err(error) => {
            return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
        }
    };
    let resolved = match crate::provider::resolve(
        &config,
        path.as_deref(),
        crate::provider::Flags {
            provider: Some(name),
            ..Default::default()
        },
    ) {
        Ok(resolved) => resolved,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    let backend = match crate::backend_for(&resolved) {
        Ok(backend) => backend,
        Err(error) => {
            return (StatusCode::UNPROCESSABLE_ENTITY, format!("{error:#}")).into_response();
        }
    };

    // A llama.cpp or mlx engine serves the one model it was started on, so
    // asking it would list one. What it *can* serve is every model on the
    // machine it loads — and a session that picks another restarts it there
    // (`engines::for_run`) — so that is the list, the engine's own model
    // first, and the ones it cannot load beside it, greyed, with why.
    let local_engine = resolved
        .engine
        .as_ref()
        .filter(|(_, e)| e.kind != crate::provider::EngineKind::Ollama)
        .map(|(_, e)| e.clone());
    let mut labels = std::collections::BTreeMap::new();
    let mut unavailable = Vec::new();
    let (models, reason) = match &local_engine {
        Some(engine) => {
            let catalog = tokio::task::spawn_blocking(crate::models::catalog)
                .await
                .unwrap_or_default();
            let mut models: Vec<String> = Vec::new();
            if let Some(own) = &engine.model {
                models.push(own.clone());
                labels.insert(own.clone(), crate::models::label(own));
            }
            for model in catalog {
                match model.runs_on(engine.kind) {
                    Ok(()) => {
                        if !models.contains(&model.reference) {
                            models.push(model.reference.clone());
                        }
                        labels.insert(model.reference, model.label);
                    }
                    Err(reason) => unavailable.push(Unavailable {
                        model: model.reference,
                        label: model.label,
                        reason,
                    }),
                }
            }
            let reason = models.is_empty().then(|| {
                format!(
                    "no model this {} engine loads in ollama, llama.cpp, Hugging Face or luu's \
                     own models directory",
                    engine.kind.as_str()
                )
            });
            (models, reason)
        }
        None => match backend.models().await {
            Ok(models) => {
                // Named by where they came from, the way the catalog is.
                if resolved.kind == crate::provider::BackendKind::Ollama {
                    for model in &models {
                        labels.insert(model.clone(), format!("{model} (ollama)"));
                    }
                }
                (models, None)
            }
            Err(error) => (Vec::new(), Some(error.to_string())),
        },
    };

    // Last used with *this profile*, then what the profile itself says, then
    // whatever the provider listed first. The first of those is why the store
    // keeps a `provider` column at all.
    let last = match &state.app.store {
        Some(store) => store.lock().await.last_model(name).unwrap_or_default(),
        None => None,
    };
    let (suggested, suggested_from) =
        match last.filter(|model| models.is_empty() || models.contains(model)) {
            Some(model) => (Some(model), Some("last used")),
            None => match config.profiles().get(name).and_then(|p| p.model.clone()) {
                Some(model) => (Some(model), Some("the profile")),
                None => match models.first() {
                    Some(model) => (Some(model.clone()), Some("the provider's list")),
                    None => (None, None),
                },
            },
        };

    Json(ProviderModels {
        models,
        reason,
        suggested,
        suggested_from,
        labels,
        unavailable,
    })
    .into_response()
}

/// Why a write was refused, in a shape an editor can act on.
///
/// **The rule lives in one place and it is not this one.** The browser never
/// decides whether a URL is off the machine; it writes what it was given, is
/// refused by the loader, and is told which host to have somebody type back.
/// A second implementation of `is_this_machine` in JavaScript is how the two
/// drift until the page cheerfully saves a file the next run cannot load.
#[derive(serde::Serialize)]
struct WriteRefused {
    message: String,
    /// Set only for the one refusal a person can answer: a default that leaves
    /// this machine and has not said so. The host is what they type.
    declaration: Option<Declaration>,
}

#[derive(serde::Serialize)]
struct Declaration {
    profile: String,
    host: String,
}

#[derive(serde::Deserialize)]
struct ProvidersEdit {
    default: Option<String>,
    #[serde(default)]
    providers: std::collections::BTreeMap<String, crate::provider::Profile>,
}

async fn put_providers(
    State(state): State<AppRouterState>,
    Json(edit): Json<ProvidersEdit>,
) -> Response {
    if !state.providers_editable {
        return (
            StatusCode::FORBIDDEN,
            "providers are read-only on a server bound off loopback",
        )
            .into_response();
    }
    let Some(path) = config_path().await else {
        return (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to write config.toml. \
             Run luu once on a terminal, or set LUU_HOME.",
        )
            .into_response();
    };
    // Built from the file as it is on disk, so the postures it names survive an
    // edit to the providers: the page writes one table and must not delete the
    // other. A file that will not load is not a reason to refuse the write
    // either — the writer checks that itself, with the loader's own rules.
    let current = load_config()
        .await
        .map(|(config, _)| config)
        .unwrap_or_default();
    let config = current.from_parts(edit.default, edit.providers);
    // Checked by the loader that will read it back, so the rule a remote
    // default has to declare itself is enforced here by being the same rule.
    if let Err(error) = write_config(config.clone(), path).await {
        let declaration = match &error {
            crate::provider::ConfigError::UndeclaredRemoteDefault { name, url, .. } => {
                Some(Declaration {
                    profile: name.clone(),
                    host: crate::provider::authority_of(url).to_string(),
                })
            }
            _ => None,
        };
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(WriteRefused {
                message: error.to_string(),
                declaration,
            }),
        )
            .into_response();
    }
    follow_the_default(&state, config.default_name()).await;
    match providers_view(&state).await {
        Ok(view) => Json(view).into_response(),
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    }
}

/// Asks the live session's destination which window it serves, in the
/// background, where nothing declared one — and takes the answer if the session
/// is still pointed there when it comes. An engine may still be loading, and
/// the engine's backend waits for it before asking.
///
/// Taking it is a mid-session change like any other: the destination gets the
/// window, and the stream gains a header carrying it, because two stretches of
/// one session under different windows are not comparable and only a header
/// says where the second began. See
/// `RECORD/2026-09-30.the-window-the-server-says.completed.md`.
fn learn_window(app: Arc<App>) {
    tokio::spawn(async move {
        let asked = app.destination().await;
        if asked.budget.limit.is_some() {
            return;
        }
        let window = match asked.backend.window().await {
            Ok(Some(window)) => window,
            _ => return,
        };
        let sending = {
            let mut destination = app.destination.write().await;
            // Moved while the server was answering: the answer was about the
            // destination this session has left.
            if !Arc::ptr_eq(&destination, &asked) {
                return;
            }
            let sending = Arc::new(asked.windowed(window));
            *destination = sending.clone();
            sending
        };
        tracing::info!(
            target: "luu::session",
            window,
            profile = sending.settings.profile.as_deref(),
            model = sending.model.as_str(),
            "the server says its window",
        );
        let posture = app.agency().await.posture(app.posture.lock().await.clone());
        let started_at = app.view.lock().await.started_at;
        if app.store.is_some() {
            app.stream.lock().await.push(crate::session::header(
                sending.backend.name(),
                &sending.model,
                sending.budget,
                sending.counter.id(),
                Some(posture),
                &sending.authority,
                sending.tool_calls,
                started_at,
            ));
        }
        app.checkpoint().await;
    });
}

/// Stops the engines this server started that the live session's destination
/// does not use: a session that moved elsewhere should not leave a model
/// loaded in memory until `serve` exits. See [`crate::engines::Supervisor::stop_others`].
async fn release_engines(app: &App, profile: Option<&str>) {
    let config = load_config().await.ok().map(|(config, _)| config);
    let keep = profile.and_then(|profile| config?.profiles().get(profile)?.engine.clone());
    app.engines.stop_others(keep.as_deref()).await;
}

/// A new `default`, taken up by the live session without restarting `serve`
/// — where nobody chose where it sends, nothing is running, and it has no
/// turns yet. A session *with* turns stays where it is: moving a conversation
/// to another model is a choice with a header of its own ("Continue here"),
/// not a side effect of saving a file, and `ProvidersView::waiting` says so.
async fn follow_the_default(state: &AppRouterState, default: Option<&str>) {
    let app = &state.app;
    let current = app.destination().await;
    if !current.settings.follows_default || current.settings.profile.as_deref() == default {
        return;
    }
    {
        let session = app.session.lock().await;
        if session.current.is_some() || session.pending.is_some() || session.next_turn > 1 {
            return;
        }
    }
    // The same path `POST /api/sessions` with no body takes, which follows the
    // file for exactly this kind of destination. A failure — an engine that
    // will not start — leaves the session where it was, and the page reads
    // `waiting` rather than a refusal of a write that did succeed.
    let _ = create_session(State(state.clone()), axum::body::Bytes::new()).await;
}

/// The `[resend]` table, as an editor sees it and as the live session is under.
///
/// Two halves because they can disagree and the disagreement is the point: the
/// file is *this machine's default* and outlives every session, while a session
/// may have been started under something else — the same relationship
/// `ProvidersView::running` has with `default`, one table along.
#[derive(serde::Serialize)]
struct ResendView {
    /// Where the file is, or would be written.
    path: Option<String>,
    editable: bool,
    refused: Option<String>,
    /// What the file says, key by key, with `None` on one it does not name.
    file: crate::provider::Resend,
    /// What the live session is actually rendering its window under.
    running: RunningResend,
    /// Rules the file now names that the **live** session was not moved onto,
    /// with the reason. Empty where there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    waiting: Vec<String>,
}

#[derive(serde::Serialize)]
struct RunningResend {
    repeat: agent_core::context::Repeat,
    prune: agent_core::context::Prune,
    results: agent_core::context::Results,
}

impl RunningResend {
    fn of(budget: Budget) -> Self {
        Self {
            repeat: budget.repeat,
            prune: budget.prune,
            results: budget.results,
        }
    }
}

async fn resend_view(state: &AppRouterState, waiting: Vec<String>) -> ResendView {
    // A file that will not load is not a reason to refuse to say what the live
    // session is under: that half is in memory and always answerable.
    let (file, path) = match load_config().await {
        Ok((config, path)) => (config.resend().unwrap_or_default(), path),
        Err(_) => (Default::default(), None),
    };
    ResendView {
        path: shown_path(path).await,
        editable: state.providers_editable,
        refused: match state.providers_editable {
            true => None,
            false => Some(
                "this server is bound off loopback. A resend rule outlives the session, and \
                 the bearer token says who may reach the port rather than who may decide what \
                 this machine sends. Edit config.toml on the machine itself."
                    .to_string(),
            ),
        },
        file,
        running: RunningResend::of(state.app.destination().await.budget),
        waiting,
    }
}

async fn get_resend(State(state): State<AppRouterState>) -> Response {
    Json(resend_view(&state, Vec::new()).await).into_response()
}

/// Writes `[resend]`, and moves the live session onto what is safe to move it
/// onto.
///
/// The two guards are the providers route's, because it is the same kind of
/// write: a 403 off loopback, and the file rebuilt from what is on disk so that
/// saving one table does not delete the others.
///
/// **What it applies live is narrower than what it writes, and that is the
/// whole of §fourth.** The two ratchets in this window only move forward, so
/// the three rules are not one case:
///
/// - **Rule A** stores nothing — its owner is chosen inside the window being
///   rendered — so it is clean in both directions and is applied as asked.
/// - **Rule B off** does not give back what is already behind the prune line:
///   the policy gates the decision and the line gates the render, and they are
///   not the same gate. Turning it off mid-session stops the line advancing and
///   returns nothing, which is a control that reports a change it did not make.
/// - **Rule C off** is worse than a no-op: handing tool output back makes the
///   prompt bigger, and the way this window absorbs bigger is the floor — which
///   drops turns with their questions and answers, and does not come back.
///
/// So B and C are applied live only when they are being turned **on**, and
/// turning either off is written to the file and waits for the next session,
/// which the response names rather than leaving to be discovered. See
/// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md` §fourth.
/// What a saved `[resend]` moves the **live** session onto, and what has to
/// wait for the next one.
///
/// Split out of the route because it is the part with an argument behind it:
/// the route writes a file, and this decides what a running conversation may be
/// told about it without costing it turns. See [`put_resend`] for that
/// argument, and `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`
/// §fourth for where it was made.
fn resend_live(asked: crate::provider::Resend, live: Budget) -> (Budget, Vec<String>) {
    use agent_core::context::Prune;

    let mut waiting = Vec::new();
    let budget = Budget {
        // Rule A stores nothing, so it moves both ways.
        repeat: asked.repeat.unwrap_or(live.repeat),
        prune: match asked.prune {
            Some(Prune::Never) if live.prune == Prune::Behind => {
                waiting.push(
                    "prune: the line a prune moved is a ratchet, so turning it off now would \
                     return nothing that is already behind it. The file says never; this \
                     session keeps pruning until the next one starts."
                        .to_string(),
                );
                live.prune
            }
            other => other.unwrap_or(live.prune),
        },
        results: match asked.results {
            // Any move that hands output *back* waits, not only the whole way
            // to `Kept`: `Cited` to `CitedReads` returns every command's output
            // to the prompt, which is the biggest single thing this rule was
            // holding out of it.
            Some(asked) if cites_less(asked, live.results) => {
                waiting.push(format!(
                    "results: handing tool output back mid-session makes the prompt bigger, and \
                     what absorbs bigger is the floor — which drops whole turns and does not \
                     come back. The file says {}; this session keeps citing at {} until the \
                     next one starts.",
                    spelled(asked),
                    spelled(live.results),
                ));
                live.results
            }
            other => other.unwrap_or(live.results),
        },
        ..live
    };
    (budget, waiting)
}

/// Whether moving to `asked` would put tool output back into the prompt.
///
/// The three values are a ladder on how much is cited — `Kept` cites nothing,
/// `CitedReads` cites what could be read again, `Cited` cites everything — and
/// only the way *down* it costs anything, because only that direction gives a
/// running session something it then has to find room for.
fn cites_less(asked: agent_core::context::Results, live: agent_core::context::Results) -> bool {
    use agent_core::context::Results;

    let rung = |results| match results {
        Results::Kept => 0,
        Results::CitedReads => 1,
        Results::Cited => 2,
    };
    rung(asked) < rung(live)
}

/// A `Results` as the wire and the file spell it, for a message a person reads.
fn spelled(results: agent_core::context::Results) -> &'static str {
    use agent_core::context::Results;
    match results {
        Results::Kept => "kept",
        Results::CitedReads => "cited_reads",
        Results::Cited => "cited",
    }
}

async fn put_resend(
    State(state): State<AppRouterState>,
    Json(asked): Json<crate::provider::Resend>,
) -> Response {
    let app = &state.app;
    if !state.providers_editable {
        return (
            StatusCode::FORBIDDEN,
            "resend rules are read-only on a server bound off loopback",
        )
            .into_response();
    }
    let Some(path) = config_path().await else {
        return (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to write config.toml. \
             Run luu once on a terminal, or set LUU_HOME.",
        )
            .into_response();
    };
    // Built from the file as it is on disk, for the reason the providers writer
    // is: the page writes one table and must not delete the rest.
    let current = load_config()
        .await
        .map(|(config, _)| config)
        .unwrap_or_default();
    if let Err(error) = write_config(current.with_resend(asked), path).await {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }

    // And now the live session, which is the half §third added to this part:
    // the mechanism for changing it mid-stream already existed and had never
    // been handed these three.
    let live = app.destination().await;
    let (budget, waiting) = resend_live(asked, live.budget);

    if budget != live.budget {
        let sending = Arc::new(live.resending(budget));
        *app.destination.write().await = sending.clone();
        // The line that says the rest of this stream was rendered under
        // something else. Built before the stream lock, because nothing in this
        // file holds two locks across an await.
        let posture = app.agency().await.posture(app.posture.lock().await.clone());
        let header = {
            let view = app.view.lock().await;
            retarget_header(&view, &sending, sending.counter.id(), posture)
        };
        if let Some(header) = header {
            // Appended rather than replacing: this is one session continuing
            // under new terms, not a new one. `create_session` clears the
            // stream because it is starting one.
            if app.store.is_some() {
                app.stream.lock().await.push(header);
            }
            let mut view = app.view.lock().await;
            view.repeat = Some(budget.repeat);
            view.prune = Some(budget.prune);
            view.results = Some(budget.results);
        }
        // Written down now rather than at the next turn's checkpoint, for the
        // reason a resume is: a session moved and then left alone would lose
        // the fact.
        app.checkpoint().await;
    }

    Json(resend_view(&state, waiting).await).into_response()
}

/// The `[authority]` table, [`ResendView`]'s mirror one table along.
///
/// No `waiting` here — [`NewSession::authority`]'s own comment gives the
/// reason: a note has no window's worth of eviction and pruning state a
/// mid-session move could leave inconsistent, so there is nothing this half
/// has to decide is unsafe to apply.
#[derive(serde::Serialize)]
struct AuthorityView {
    path: Option<String>,
    editable: bool,
    refused: Option<String>,
    file: crate::provider::AuthorityNotes,
    running: RunningAuthority,
}

#[derive(serde::Serialize)]
struct RunningAuthority {
    draft: Option<agent_core::sandbox::AuthorityNote>,
    plan: Option<agent_core::sandbox::AuthorityNote>,
}

impl RunningAuthority {
    fn of(authority: &crate::provider::AuthorityNotes) -> Self {
        Self {
            draft: authority.draft_note(),
            plan: authority.plan_note(),
        }
    }
}

async fn authority_view(state: &AppRouterState) -> AuthorityView {
    let (file, path) = match load_config().await {
        Ok((config, path)) => (config.authority().cloned().unwrap_or_default(), path),
        Err(_) => (Default::default(), None),
    };
    AuthorityView {
        path: shown_path(path).await,
        editable: state.providers_editable,
        refused: match state.providers_editable {
            true => None,
            false => Some(
                "this server is bound off loopback. An authority note outlives the session, and \
                 the bearer token says who may reach the port rather than who may decide what \
                 this machine tells a model. Edit config.toml on the machine itself."
                    .to_string(),
            ),
        },
        file,
        running: RunningAuthority::of(&state.app.destination().await.authority),
    }
}

async fn get_authority(State(state): State<AppRouterState>) -> Response {
    Json(authority_view(&state).await).into_response()
}

/// Writes `[authority]`, and moves the live session onto it outright.
///
/// **Applied live in full, unlike [`put_resend`]**: an authority note is
/// recomputed from nothing on every `select`, so there is no state a move
/// could leave stranded and no direction that costs a turn to reverse. What
/// [`put_resend`] spends a `waiting` message explaining is, for this table,
/// simply true every time.
async fn put_authority(
    State(state): State<AppRouterState>,
    Json(asked): Json<crate::provider::AuthorityNotes>,
) -> Response {
    let app = &state.app;
    if !state.providers_editable {
        return (
            StatusCode::FORBIDDEN,
            "authority notes are read-only on a server bound off loopback",
        )
            .into_response();
    }
    let Some(path) = config_path().await else {
        return (
            StatusCode::CONFLICT,
            "this machine has no state directory yet, so there is nowhere to write config.toml. \
             Run luu once on a terminal, or set LUU_HOME.",
        )
            .into_response();
    };
    let current = load_config()
        .await
        .map(|(config, _)| config)
        .unwrap_or_default();
    if let Err(error) = write_config(current.with_authority(asked.clone()), path).await {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }

    let live = app.destination().await;
    // Field by field, [`resend_live`]'s rule A: a table naming one authority
    // leaves the other exactly as it was running, rather than clearing it.
    let authority = crate::provider::AuthorityNotes {
        draft: asked.draft.or_else(|| live.authority.draft.clone()),
        plan: asked.plan.or_else(|| live.authority.plan.clone()),
    };
    if authority != live.authority {
        let sending = Arc::new(live.noting(authority));
        *app.destination.write().await = sending.clone();
        let posture = app.agency().await.posture(app.posture.lock().await.clone());
        let header = {
            let view = app.view.lock().await;
            retarget_header(&view, &sending, sending.counter.id(), posture)
        };
        if let Some(header) = header {
            if app.store.is_some() {
                app.stream.lock().await.push(header);
            }
            let mut view = app.view.lock().await;
            view.authority_draft = sending.authority.draft_note();
            view.authority_plan = sending.authority.plan_note();
            view.tool_calls = sending
                .tool_calls
                .native()
                .then(|| sending.tool_calls.as_str().to_string());
        }
        app.checkpoint().await;
    }

    Json(authority_view(&state).await).into_response()
}

async fn list_sessions(State(state): State<AppRouterState>) -> Response {
    let live = state.app.view.lock().await.summary();
    let mut sessions = vec![live];
    let active_id = state.app.session_id.lock().await.clone();
    if let Some(store) = &state.app.store {
        match store.lock().await.list() {
            Ok(stored) => sessions.extend(stored.into_iter().filter(|row| row.id != active_id)),
            Err(error) => eprintln!("warning: could not list the session store: {error:#}"),
        }
    }
    Json(sessions).into_response()
}

/// What a new session may choose: where it sends, which model there, and what
/// it is allowed to do while it is there.
///
/// All optional and all absent is what every client sent before these existed,
/// and it keeps meaning *the session this server is already pointed at*.
#[derive(Debug, Default, serde::Deserialize)]
struct NewSession {
    /// A profile name out of `config.toml`. Never a URL: what may be chosen is
    /// bounded by what somebody wrote on this machine.
    provider: Option<String>,
    model: Option<String>,
    /// A posture name out of the same file. Never a path, and never a runtime
    /// and an image, for the same reason `provider` is never a URL. See
    /// `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
    posture: Option<String>,
    /// The posture on another container runtime — a word out of five, never
    /// an image. See [`posture_for`].
    #[serde(default)]
    runtime: Option<String>,
    /// The three resend rules, each absent meaning *whatever this server is
    /// already under* — its flags, or the rules the session before it chose.
    ///
    /// Named by the header's own words (`always`/`once`, `never`/`behind`,
    /// `kept`/`cited`) so that the request that started a session and the
    /// recording of it say the arm the same way. Bounded by the enum, which is
    /// the same guard `provider` and `posture` get from being names out of a
    /// file: what may be asked for is what this machine has.
    ///
    /// Not refused on a resume, deliberately. It was going to be — the posture
    /// refuses one — and the refusal was withdrawn because it was borrowed
    /// along with the shape: a posture may not move because *its jobs were
    /// approved under it*, and these rules grant nothing and bound nothing.
    /// See `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`
    /// §third.
    #[serde(default)]
    repeat: Option<agent_core::context::Repeat>,
    #[serde(default)]
    prune: Option<agent_core::context::Prune>,
    #[serde(default)]
    results: Option<agent_core::context::Results>,
    /// What a draft or a plan this session opens is told about itself, each
    /// absent meaning *whatever this server is already under* — the same
    /// three-level precedence [`NewSession::resend`] gives `repeat`, `prune`
    /// and `results`, one field along. Named `authority` and not `notes`,
    /// because it is a position on `agent_core::sandbox::Authority` and not a
    /// scratchpad. See
    /// `RECORD/2026-09-22.an-authority-a-model-is-told.completed.md`.
    #[serde(default)]
    authority: Option<crate::provider::AuthorityNotes>,
}

impl NewSession {
    /// The budget this session asked for, out of this machine's table and the
    /// one in place.
    ///
    /// Three levels, and the middle one is what part 3 added: **what the
    /// session asked for**, else **what `[resend]` says**, else **what is
    /// already running**.
    ///
    /// The file in the middle rather than nowhere, because the table's whole
    /// job is to be *what a session starts on unless it says otherwise* — and
    /// because without it a rule turned off from the page could never take
    /// effect. Turning rule B off does not move a running session (the line is
    /// a ratchet), so if a new session then inherited the running session's
    /// rules too, the only way to act on the write would be to restart the
    /// binary, and `put_resend`'s *until the next one starts* would be a lie.
    ///
    /// Where the file says nothing about a rule, what is in place carries — the
    /// part 2 behaviour, unchanged, because a machine that states no position
    /// is not a machine stating the default.
    ///
    /// `None` where nothing moved, so that the destination already resolved is
    /// handed on untouched rather than rebuilt into an equal one.
    fn resend(&self, machine: crate::provider::Resend, budget: Budget) -> Option<Budget> {
        let asked = Budget {
            repeat: self.repeat.or(machine.repeat).unwrap_or(budget.repeat),
            prune: self.prune.or(machine.prune).unwrap_or(budget.prune),
            results: self.results.or(machine.results).unwrap_or(budget.results),
            ..budget
        };
        (asked != budget).then_some(asked)
    }

    /// [`Self::resend`]'s three levels, for the two notes instead of the three
    /// rules. **Simpler by one level in effect, not in code**: a rule has a
    /// window's worth of eviction and pruning state that a rename mid-session
    /// could leave inconsistent, which is what `resend_live` exists to reason
    /// about. A note has none — it is recomputed from nothing every render, so
    /// there is no "safe to move a live session onto" question to answer, and
    /// what is "already running" is simply carried forward like any other
    /// field nobody named.
    fn authority(
        &self,
        machine: crate::provider::AuthorityNotes,
        running: crate::provider::AuthorityNotes,
    ) -> Option<crate::provider::AuthorityNotes> {
        let asked = self.authority.clone().unwrap_or_default();
        let merged = crate::provider::AuthorityNotes {
            draft: asked
                .draft
                .or(machine.draft)
                .or_else(|| running.draft.clone()),
            plan: asked.plan.or(machine.plan).or_else(|| running.plan.clone()),
        };
        (merged != running).then_some(merged)
    }
}

/// This machine's `[resend]` table, re-read at a session start.
///
/// Read here rather than held from startup, and it is the posture's own
/// precedent: `posture_for` re-resolves the policy file so that a new session
/// picks up an edited `luu.toml`. The same applies with more force here,
/// because this file is one the page itself writes — a table saved at turn 12
/// that only reached sessions started before it would be a control that does
/// nothing.
///
/// A file that will not load says nothing, which is not the same as a file that
/// says the defaults: it is already reported where the providers are, and a
/// machine with an unreadable config is not a machine that chose `always`.
async fn machine_resend() -> crate::provider::Resend {
    load_config()
        .await
        .map(|(config, _)| config.resend().unwrap_or_default())
        .unwrap_or_default()
}

/// This machine's `[authority]` table, [`machine_resend`]'s reason exactly,
/// one table along.
async fn machine_authority() -> crate::provider::AuthorityNotes {
    load_config()
        .await
        .map(|(config, _)| config.authority().cloned().unwrap_or_default())
        .unwrap_or_default()
}

/// The body both session routes take, which is allowed to be absent.
///
/// Not `Json<NewSession>`: a client that posts no body at all — which is every
/// client written before this parameter existed — would be answered with a 415
/// about a missing content type rather than with the session it asked for.
fn asked_for(body: &axum::body::Bytes) -> Result<NewSession, (StatusCode, String)> {
    match body.is_empty() {
        true => Ok(NewSession::default()),
        false => serde_json::from_slice(body)
            .map_err(|error| (StatusCode::BAD_REQUEST, format!("{error}"))),
    }
}

/// The posture a new session asked for, resolved into the agency that is it.
///
/// Nothing is swapped here: like the destination, it is built **before**
/// anything is reset, so a posture whose image is not built or whose runtime is
/// not installed leaves the session that is running exactly as it was.
///
/// A session that names none still gets a fresh one, out of the policy file
/// this server was started with — so a new session picks up an edited
/// `luu.toml`, and never inherits the container the previous one was using.
///
/// `runtime` is a **variant**: the same posture on another container runtime.
/// It is a name out of a closed set — the five `Runtime::CONTAINED` — and it
/// replaces the runtime of a posture that is already contained, keeping its
/// image and its in-image paths; it can never put a contained posture on the
/// host, nor a host posture in a container. That is how it keeps the rule
/// [`NewSession::posture`] states, that the browser never types a runtime and
/// an image: it picks one of five words compiled into luu, and the image is
/// still the file's. See
/// `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
async fn posture_for(
    app: &App,
    asked: Option<&str>,
    runtime: Option<&str>,
) -> Result<(Option<String>, Arc<Agency>), (StatusCode, String)> {
    let Some(build) = &app.agency_for else {
        if let Some(runtime) = runtime {
            return Err((
                StatusCode::CONFLICT,
                format!("this surface cannot choose a posture, so not `{runtime}` either"),
            ));
        }
        return match asked {
            None => Ok((app.posture.lock().await.clone(), app.agency().await)),
            Some(name) => Err((
                StatusCode::CONFLICT,
                format!(
                    "this surface cannot choose a posture, so `{name}` cannot be honoured here:                      over stdio the process is the session, and its policy file is a flag"
                ),
            )),
        };
    };
    let policy = match asked {
        None => None,
        Some(name) => match app.postures().get(name) {
            Some(posture) => Some(posture.policy.clone()),
            None => {
                let where_from = app
                    .postures_path
                    .clone()
                    .unwrap_or_else(|| "config.toml".to_string());
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("there is no [posture.{name}] in {where_from}"),
                ));
            }
        },
    };
    let runtime = match runtime {
        None => None,
        Some(text) => {
            let runtime = text
                .parse::<agent_core::worker::Runtime>()
                .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
            if !runtime.is_contained() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "`{runtime}` is not a container runtime: a variant moves a contained \
                         posture between runtimes, and a posture that runs on the host is \
                         chosen by its own name"
                    ),
                ));
            }
            let declared = app
                .describe_for
                .as_ref()
                .map(|describe| describe(policy.clone(), None).runtime);
            let contained = declared
                .as_deref()
                .and_then(|declared| declared.parse::<agent_core::worker::Runtime>().ok())
                .is_some_and(|declared| declared.is_contained());
            if !contained {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "posture {} runs on {}, and only a contained posture moves to another \
                         container runtime: its sandbox names paths on this machine, not in an image",
                        asked.unwrap_or("(the server's own)"),
                        declared.as_deref().unwrap_or("an unknown runtime"),
                    ),
                ));
            }
            Some(runtime)
        }
    };
    let agency = build(policy, runtime)
        .await
        .map_err(|error| (StatusCode::BAD_REQUEST, format!("{error:#}")))?;
    Ok((asked.map(str::to_string), Arc::new(agency)))
}

/// The line that says the rest of this stream was produced somewhere else.
///
/// `None` when it would say nothing new. The comparison is against the **fold**
/// — which is what the stream's last header folded to — and not against the
/// destination the server happens to be pointed at, so a plain resume that
/// inherits a different destination records that too. It was doing it silently
/// before this existed.
///
/// `started_at` is the session's own, never the moment of the resume: every
/// `at_ms` in a stream is relative to the first header's, which is what the
/// field means.
///
/// **The posture is a third term and not a passenger.** It was one until
/// 2026-09-17: a resume from a container onto a host, on the same model, moved
/// nothing this function compared and so wrote nothing, and the rest of the
/// stream then read as though it had stayed contained. `resume_session` now
/// refuses that case outright, which leaves this reachable only where the
/// stored posture is unknown — and a recording's honesty should not be a
/// downstream consequence of an access check somebody may relax later. See
/// `RECORD/2026-09-17.what-an-approval-was-granted-under.completed.md`.
fn retarget_header(
    view: &SessionView,
    sending: &Destination,
    counter: agent_core::context::Counter,
    posture: record::Posture,
) -> Option<record::RecordLine> {
    let same = view.backend == sending.backend.name()
        && view.model == sending.model
        && view
            .posture
            .as_ref()
            .is_some_and(|stored| stored.same_place(&posture))
        // The fourth term, and it is here for the reason the posture became the
        // third: a resume may move these — nothing was approved under them — and
        // a move this function does not compare is a move the stream does not
        // record, which is exactly how a posture that changed on a resume wrote
        // no line for a day. `None` is a stream from before format 11, which
        // does not say what it ran under, so it cannot be *the same* as anything
        // and the line is written.
        && (view.repeat, view.prune, view.results)
            == (
                Some(sending.budget.repeat),
                Some(sending.budget.prune),
                Some(sending.budget.results),
            )
        // A fifth term, `authority`'s own reason: a session whose notes moved
        // is a session rendering under something else, the same fact a rule
        // moving already is.
        && (view.authority_draft.clone(), view.authority_plan.clone())
            == (
                sending.authority.draft_note(),
                sending.authority.plan_note(),
            )
        // And the transport, a sixth: a session whose tools moved from the
        // system text to the request's own field is rendering under something
        // else, the same fact a rule moving is. `None` reads as fenced, which
        // is what every stream before `record::FORMAT` 19 was.
        && view.tool_calls.as_deref().unwrap_or("fenced") == sending.tool_calls.as_str();
    match same {
        true => None,
        false => Some(crate::session::header(
            sending.backend.name(),
            &sending.model,
            sending.budget,
            counter,
            Some(posture),
            &sending.authority,
            sending.tool_calls,
            view.started_at,
        )),
    }
}

/// The destination a new session asked for, built out of the providers file.
///
/// Everything a destination decides is rebuilt together — see [`Destination`] —
/// and everything the *process* decided (the sandbox, the map, the store, the
/// tokens a turn may select) is carried over from the one in place.
async fn destination_for(
    app: &App,
    current: &Destination,
    asked: &NewSession,
    editable: bool,
) -> Result<Destination, (StatusCode, String)> {
    let (config, path) = load_config()
        .await
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
    let resolved = crate::provider::resolve(
        &config,
        path.as_deref(),
        crate::provider::Flags {
            provider: asked.provider.as_deref(),
            ..Default::default()
        },
    )
    .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;

    // The one authority rule this route has, and it is narrower than the write
    // route's. Writing a `default` outlives the session and the gate, so it is
    // loopback-only; a session's destination dies with the session. What the
    // bearer token must still not buy is *this machine's prompts leaving it* to
    // a destination whoever holds the token picked — so off loopback the choice
    // is bounded to profiles that stay here. Decided by `is_this_machine`,
    // server-side, in the one place it is implemented.
    if !editable && !resolved.url.is_empty() && !crate::provider::is_this_machine(&resolved.url) {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "{} sends off this machine, and this server is bound off loopback: a token says \
                 who may reach the port, not where this machine's prompts may go. Choose it from \
                 a browser on the machine itself.",
                asked.provider.as_deref().unwrap_or("that provider"),
            ),
        ));
    }

    let mut resolved = resolved;
    if let Some(model) = asked
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        resolved.model = model.to_string();
    }
    // A profile that names an engine is one luu starts, on the model asked
    // for — which for a llama.cpp engine may mean restarting it on another.
    // **Not waited for here**: loading a 14B takes tens of seconds, and a
    // request that held the page for all of them was a page that looked
    // frozen. The backend is wrapped so every call waits for the engine, and
    // the caller warms it once the session is pointed there. After the model
    // override for that reason. See
    // `RECORD/2026-09-29.a-switch-that-does-not-wait.completed.md`.
    let engine = crate::engines::for_run(&resolved);
    let backend = crate::backend_for(&resolved)
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, format!("{error:#}")))?;
    let backend: Box<dyn Backend> = match &engine {
        Some((name, engine)) => Box::new(crate::engines::Started::new(
            backend.into(),
            app.engines.clone(),
            name.clone(),
            engine.clone(),
        )),
        None => backend,
    };
    // Written back through the same function the CLI uses, so the page, the
    // header and the turn all name the model the same way.
    resolved.model = crate::model_for(backend.as_ref(), resolved.model);
    let (counter, counter_warning) =
        crate::session::counter_for(&resolved.model, app.tokenizer.as_deref())
            .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, format!("{error:#}")))?;
    // The profile's window, and everything else about the budget kept: the
    // reserve, the eviction policy and the repeat rule are the run's, not the
    // destination's.
    let budget = Budget {
        limit: (resolved.context_limit > 0).then_some(resolved.context_limit),
        ..current.budget
    };
    let settings = current
        .settings
        .pointed_at(&resolved, budget, counter.id(), counter_warning);
    Ok(Destination {
        backend: backend.into(),
        model: resolved.model,
        budget,
        counter,
        settings,
        engine,
        tool_calls: resolved.tool_calls,
        // Neither the destination's nor the process's, `budget`'s own reason
        // one field up: kept from the session that is being pointed elsewhere.
        authority: current.authority.clone(),
    })
}

async fn create_session(State(state): State<AppRouterState>, body: axum::body::Bytes) -> Response {
    let app = &state.app;
    let asked = match asked_for(&body) {
        Ok(asked) => asked,
        Err((status, message)) => return (status, message).into_response(),
    };

    {
        let session = app.session.lock().await;
        if session.current.is_some() || session.pending.is_some() {
            return (StatusCode::CONFLICT, "a turn is currently running").into_response();
        }
    }

    app.checkpoint().await;

    // Resolved before anything is reset: a session that cannot be pointed
    // anywhere is one that should not have ended the previous one.
    let sending = match (asked.provider.is_some(), asked.model.is_some()) {
        // Nothing named: keep what is in place — unless nobody chose that
        // either, in which case it is the file's default, and the file is read
        // again so a default written since `serve` started is the one used.
        // `true` for editable: following the file is not a token holder
        // picking a destination, which is what that check is about.
        (false, false) => {
            let current = app.destination().await;
            match current.settings.follows_default {
                false => current,
                true => match destination_for(app, &current, &asked, true).await {
                    Ok(next) => Arc::new(next),
                    Err((status, message)) => return (status, message).into_response(),
                },
            }
        }
        _ => {
            let current = app.destination().await;
            match destination_for(app, &current, &asked, state.providers_editable).await {
                Ok(next) => Arc::new(next),
                Err((status, message)) => return (status, message).into_response(),
            }
        }
    };
    // And the rules the window is rendered under, which are the session's and
    // neither the destination's nor the process's. Applied after the
    // destination so that naming a rule alone needs no profile to go with it,
    // and before anything is reset for the same reason everything else here is.
    // Until this they arrived once, at `serve`, and every session the process
    // ran shared them — which is what made them unchoosable from a page. See
    // `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md` part 2.
    let sending = match asked.resend(machine_resend().await, sending.budget) {
        Some(budget) => Arc::new(sending.resending(budget)),
        None => sending,
    };
    // And what a draft or a plan this session opens is told about itself,
    // [`NewSession::authority`]'s own reason — applied the same way, beside
    // the rules and not folded into them, because it is not one of the three.
    let sending = match asked.authority(machine_authority().await, sending.authority.clone()) {
        Some(authority) => Arc::new(sending.noting(authority)),
        None => sending,
    };
    // And the same rule for what it may do: built before anything is reset, so
    // a runtime that is not installed or an image that is not built refuses the
    // *new* session rather than ending the one that is running.
    let (posture_name, agency) =
        match posture_for(app, asked.posture.as_deref(), asked.runtime.as_deref()).await {
            Ok(resolved) => resolved,
            Err((status, message)) => return (status, message).into_response(),
        };
    *app.destination.write().await = sending.clone();
    tracing::info!(
        target: "luu::session",
        profile = sending.settings.profile.as_deref(),
        model = sending.model.as_str(),
        backend = sending.backend.name(),
        tool_calls = sending.tool_calls.as_str(),
        "the live session now sends here",
    );
    release_engines(app, sending.settings.profile.as_deref()).await;
    // Loaded behind the answer rather than inside it — see `destination_for`.
    if let Some((name, engine)) = &sending.engine {
        app.engines.warm(name, engine);
    }
    learn_window(app.clone());
    // The previous posture is ended rather than dropped: the thing being ended
    // may be a container, and `kill_on_drop` would get to it eventually. Skipped
    // when the resolver handed back the one already in place, which is what a
    // surface that cannot choose a posture always gets.
    let previous = std::mem::replace(&mut *app.agency.write().await, agency.clone());
    if !Arc::ptr_eq(&previous, &agency) {
        previous.shutdown().await;
    }
    *app.posture.lock().await = posture_name.clone();

    let started_at = now_ms();
    let new_id = session_id(started_at);
    *app.session_id.lock().await = new_id.clone();
    *app.session_started_at.lock().await = started_at;
    // Built once and used twice — the stream's header and the live view — for
    // the reason `store_parity` exists: the view is not built by folding that
    // line, so the two are only equal while every field is put in both places.
    let posture = agency.posture(posture_name);
    // A new session is a new stream, and a stream starts with the header that
    // says what it is comparable with — which is why the destination is
    // swapped above this line and never below it.
    *app.stream.lock().await = vec![crate::session::header(
        sending.backend.name(),
        &sending.model,
        sending.budget,
        sending.counter.id(),
        Some(posture.clone()),
        &sending.authority,
        sending.tool_calls,
        started_at,
    )];
    // And the `--record` file gets the same line, which until now it did not:
    // one header at process start named the first session's model, posture and
    // rules for every session after it. See
    // `RECORD/2026-09-19.a-header-per-session.completed.md`.
    if let Some(recorder) = &app.recorder {
        recorder.session(
            sending.backend.name(),
            &sending.model,
            sending.budget,
            sending.counter.id(),
            Some(posture.clone()),
            &sending.authority,
            sending.tool_calls,
            started_at,
        );
    }

    {
        let mut session = app.session.lock().await;
        session.next_turn = 1;
        session.current = None;
        session.cancel = None;
        session.held = None;
        session.context = app
            .agency()
            .await
            .with_tools(AgentContext::new(SYSTEM), sending.tool_calls)
            .with_map(&app.map_rendered);
        session.prefix = PrefixTracker::default();
        session.pending = None;
        session.narrowed = None;
    }

    let summary = {
        let mut view = app.view.lock().await;
        *view = SessionView::new(LIVE_SESSION, sending.backend.name(), &sending.model);
        view.started_at = started_at;
        view.posture = Some(posture);
        view.repeat = Some(sending.budget.repeat);
        view.prune = Some(sending.budget.prune);
        view.results = Some(sending.budget.results);
        view.tool_calls = sending
            .tool_calls
            .native()
            .then(|| sending.tool_calls.as_str().to_string());
        let mut s = view.summary();
        s.id = new_id.clone();
        s
    };

    let hello = ServerMessage::Hello {
        protocol: protocol::VERSION,
        backend: sending.backend.name().to_string(),
        model: sending.model.clone(),
        turn: None,
        // The new name, not the old one: a client that signs an approval after
        // a switch signs it against the session it is now watching.
        session: Some(new_id),
    };
    app.publish(Event::Protocol(hello)).await;

    (StatusCode::CREATED, Json(summary)).into_response()
}

/// `PUT /api/session/posture`: `null` is the server's own policy file.
#[derive(serde::Deserialize)]
struct MovePosture {
    #[serde(default)]
    posture: Option<String>,
    /// A variant: see [`posture_for`].
    #[serde(default)]
    runtime: Option<String>,
}

/// Moves the live session to another posture: where its commands run, and
/// what they may do.
///
/// **Between jobs, and in writing.** What went wrong when a posture moved
/// under a session was something approved under one being carried into
/// another, and a move the stream did not record — see
/// `RECORD/2026-09-17.what-an-approval-was-granted-under.completed.md`. So a
/// move is refused while a turn runs, a proposal waits at the gate, or an
/// approved job is open, and it writes a header line carrying the new posture:
/// the fold's *last header wins* already means "from here on, this ran
/// somewhere else". A draft is not a refusal: it runs under its posture's
/// floor, which grants nothing to carry. See
/// `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
async fn put_session_posture(
    State(state): State<AppRouterState>,
    Json(asked): Json<MovePosture>,
) -> Response {
    let app = &state.app;
    // Held for the whole move, so a turn cannot start between the check and
    // the swap. Nothing below takes it.
    let session = app.session.lock().await;
    if session.current.is_some() {
        return (
            StatusCode::CONFLICT,
            "a turn is running: the session moves between turns",
        )
            .into_response();
    }
    if session.pending.is_some() {
        return (
            StatusCode::CONFLICT,
            "a proposal is waiting at the gate, asked under this posture: approve or decline it, \
             and the session moves after",
        )
            .into_response();
    }
    let approved = session.context.live_job().filter(|job| {
        session
            .context
            .job(*job)
            .is_some_and(|job| job.plan.is_some())
    });
    if let Some(job) = approved {
        return (
            StatusCode::CONFLICT,
            format!(
                "job {job} is open, and it was approved under this posture: close it, and the \
                 session moves between jobs"
            ),
        )
            .into_response();
    }

    let current = app.agency().await.posture(app.posture.lock().await.clone());
    // Built before anything is swapped, `create_session`'s discipline: a
    // runtime that is not installed refuses the move and leaves the session
    // where it was.
    let (name, agency) =
        match posture_for(app, asked.posture.as_deref(), asked.runtime.as_deref()).await {
            Ok(resolved) => resolved,
            Err((status, message)) => return (status, message).into_response(),
        };
    let posture = agency.posture(name.clone());
    if current.same_place(&posture) && current.name == posture.name {
        if !Arc::ptr_eq(&agency, &app.agency().await) {
            agency.shutdown().await;
        }
        return Json(serde_json::json!({ "posture": posture, "moved": false })).into_response();
    }

    // Ended rather than dropped, for `Agency::shutdown`'s reason — and with it
    // the old container, and any terminal that was open in it.
    let previous = std::mem::replace(&mut *app.agency.write().await, agency.clone());
    if !Arc::ptr_eq(&previous, &agency) {
        previous.shutdown().await;
    }
    *app.posture.lock().await = name.clone();
    drop(session);

    let sending = app.destination().await;
    let started_at = *app.session_started_at.lock().await;
    app.stream.lock().await.push(crate::session::header(
        sending.backend.name(),
        &sending.model,
        sending.budget,
        sending.counter.id(),
        Some(posture.clone()),
        &sending.authority,
        sending.tool_calls,
        started_at,
    ));
    if let Some(recorder) = &app.recorder {
        recorder.session(
            sending.backend.name(),
            &sending.model,
            sending.budget,
            sending.counter.id(),
            Some(posture.clone()),
            &sending.authority,
            sending.tool_calls,
            started_at,
        );
    }
    {
        // What folding that line produces, field for field — the resume's
        // `moved` branch, for `store_parity`'s reason.
        let mut view = app.view.lock().await;
        view.posture = Some(posture.clone());
        view.repeat = Some(sending.budget.repeat);
        view.prune = Some(sending.budget.prune);
        view.results = Some(sending.budget.results);
        view.authority_draft = sending.authority.draft_note();
        view.authority_plan = sending.authority.plan_note();
        view.tool_calls = sending
            .tool_calls
            .native()
            .then(|| sending.tool_calls.as_str().to_string());
    }
    // Now, not at the next turn: a session moved and then left alone must
    // still say where it went.
    app.checkpoint().await;
    tracing::info!(
        target: "luu::session",
        posture = name.as_deref(),
        runtime = posture.runtime.as_str(),
        "the live session now runs here",
    );
    Json(serde_json::json!({ "posture": posture, "moved": true })).into_response()
}

/// Picks a stored session back up — optionally somewhere else.
///
/// The body is `POST /api/sessions`', and means the same thing: a profile out
/// of the file and a model there. What differs is what comes with it — the
/// history — and that the stream gains a `Header` saying where it changed. See
/// `RECORD/2026-09-07.a-second-header.completed.md`.
async fn resume_session(
    State(state): State<AppRouterState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let app = &state.app;
    let id = bare(&id);
    let asked = match asked_for(&body) {
        Ok(asked) => asked,
        Err((status, message)) => return (status, message).into_response(),
    };
    {
        let session = app.session.lock().await;
        if session.current.is_some() || session.pending.is_some() {
            return (StatusCode::CONFLICT, "a turn is currently running").into_response();
        }
    }

    let Some(store_mutex) = &app.store else {
        return (StatusCode::NOT_IMPLEMENTED, "session store is disabled").into_response();
    };

    // Before the checkpoint and before anything is loaded: a body naming a
    // profile the file does not have leaves the session that is running exactly
    // as it was.
    let sending = match (asked.provider.is_some(), asked.model.is_some()) {
        (false, false) => app.destination().await,
        _ => {
            let current = app.destination().await;
            match destination_for(app, &current, &asked, state.providers_editable).await {
                Ok(next) => Arc::new(next),
                Err((status, message)) => return (status, message).into_response(),
            }
        }
    };
    // And the rules the window is rendered under, which are the session's and
    // neither the destination's nor the process's. Applied after the
    // destination so that naming a rule alone needs no profile to go with it,
    // and before anything is reset for the same reason everything else here is.
    // Until this they arrived once, at `serve`, and every session the process
    // ran shared them — which is what made them unchoosable from a page. See
    // `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md` part 2.
    let sending = match asked.resend(machine_resend().await, sending.budget) {
        Some(budget) => Arc::new(sending.resending(budget)),
        None => sending,
    };
    let sending = match asked.authority(machine_authority().await, sending.authority.clone()) {
        Some(authority) => Arc::new(sending.noting(authority)),
        None => sending,
    };

    app.checkpoint().await;

    // The fold on its own, and before any posture is built: resolving a name
    // can start a container, and an id nobody stored must not start one.
    let loaded_view = {
        let store = store_mutex.lock().await;
        match store.load(id) {
            Ok(Some(view)) => view,
            Ok(None) => return not_found("session"),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        }
    };

    // What this resume would run under. An absent name is the posture already
    // in place and builds nothing; a name is resolved *here*, before anything
    // is swapped, so a runtime that is not installed refuses the resume rather
    // than ending the session that is running — the discipline
    // `create_session` keeps for the same reason.
    let (posture_name, agency) =
        match posture_for(app, asked.posture.as_deref(), asked.runtime.as_deref()).await {
            Ok(resolved) => resolved,
            Err((status, message)) => return (status, message).into_response(),
        };
    let posture = agency.posture(posture_name.clone());

    // A destination may move under a history; a posture may not. What a session
    // is allowed to do is what its jobs were approved against, and handing a
    // person back a gate somewhere else replays an answer they gave about
    // somewhere else — a proposal left open under a container comes back at the
    // gate by design (`pending_proposal`), and before this it came back on
    // whatever the server happened to be running.
    //
    // Compared on the three facts and never on the name, for the reason
    // `record::Posture` carries three: the file behind a name can be edited
    // tomorrow. A stored `None` is *unknown* — a recording from before format
    // 10 — and is let through rather than guessed at, because inventing `host`
    // for it would be a fact this project does not hold; the header below then
    // says where the rest of it ran. See
    // `RECORD/2026-09-17.what-an-approval-was-granted-under.completed.md`.
    if let Some(stored) = &loaded_view.posture
        && !stored.same_place(&posture)
    {
        // Whatever was built a few lines up is not going to be used, and it may
        // be a container: ended rather than dropped, for `Agency::shutdown`'s
        // own reason.
        if !Arc::ptr_eq(&agency, &app.agency().await) {
            agency.shutdown().await;
        }
        let named = match &stored.name {
            Some(name) => format!(" `{name}`"),
            None => String::new(),
        };
        return (
            StatusCode::CONFLICT,
            format!(
                "session {id} ran under posture{named} ({}) and this server is on ({}); \
                 its jobs were approved against the first one. Start a session on that \
                 posture, or name it in this resume.",
                stored.describe(),
                posture.describe(),
            ),
        )
            .into_response();
    }

    // Read once the posture is settled and before the store lock: the tool
    // block is the same for every posture — the definitions never move, because
    // they are the second half of the cached prefix — and asking for it inside
    // the lock would be an await holding a guard that cannot cross a thread.
    let definitions = agency.definitions();
    let resumed = {
        let store = store_mutex.lock().await;
        match store.resume(
            id,
            SYSTEM,
            definitions,
            &app.map_rendered,
            sending.counter.as_ref(),
            // The posture this session is being resumed **under**, which is the
            // one settled above and not the one it ran under. A span the new
            // posture may not read is exactly the case storing bytes would have
            // walked past, so it is the sandbox to ask.
            Some(agency.sandbox.as_ref()),
        ) {
            Ok(Some(restored)) => restored,
            Ok(None) => return not_found("session"),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        }
    };
    // Said out loud, once per span, because the alternative is a session that
    // came back with a citation where a file was and nothing anywhere saying
    // why. The turn keeps its exchange either way; what it has lost is bytes
    // this posture may not read, which is a fact about the policy and not
    // about the session.
    for one in &resumed.unreadable {
        eprintln!(
            "warning: turn {} was grounded with {}, which is not readable here: {}",
            one.turn, one.spec, one.why,
        );
    }
    // The store rebuilt it with the fenced block; the destination it resumes
    // on decides which transport the tools actually travel under.
    let resumed_context = agency.with_tools(resumed.context, sending.tool_calls);

    // After the fold is rebuilt and before anything runs on it.
    *app.destination.write().await = sending.clone();
    tracing::info!(
        target: "luu::session",
        profile = sending.settings.profile.as_deref(),
        model = sending.model.as_str(),
        backend = sending.backend.name(),
        tool_calls = sending.tool_calls.as_str(),
        "the live session now sends here",
    );
    release_engines(app, sending.settings.profile.as_deref()).await;
    // Loaded behind the answer rather than inside it — see `destination_for`.
    if let Some((name, engine)) = &sending.engine {
        app.engines.warm(name, engine);
    }
    learn_window(app.clone());
    // The posture the session keeps, which is its own. A no-op when the body
    // named nothing — the resolver hands back the agency already in place, so
    // the pointers are equal and nothing is ended.
    let previous = std::mem::replace(&mut *app.agency.write().await, agency.clone());
    if !Arc::ptr_eq(&previous, &agency) {
        previous.shutdown().await;
    }
    *app.posture.lock().await = posture_name;

    *app.session_id.lock().await = id.to_string();
    // Its own clock, and its own stream: appends continue the one the store
    // already holds rather than starting a second one under the same name.
    *app.session_started_at.lock().await = loaded_view.started_at;
    // The one line this route writes. It says the rest of these turns were
    // produced somewhere else, and it is absent when they were not — built
    // before the stream lock, because nothing in this file holds two locks
    // across an await.
    let header = retarget_header(
        &loaded_view,
        &sending,
        sending.counter.id(),
        posture.clone(),
    );
    let moved = header.is_some();
    // The `--record` file gets one **unconditionally**, which is the one place
    // this route and the stream disagree on purpose. `retarget_header` asks
    // whether anything moved *since the session was stored*, and answers no for
    // a resume onto the same destination and posture. A file asks a different
    // question: what changed since the line above, and what is the base for the
    // lines below — and both changed, because the session did. See
    // `RECORD/2026-09-19.a-header-per-session.completed.md`.
    if let Some(recorder) = &app.recorder {
        recorder.session(
            sending.backend.name(),
            &sending.model,
            sending.budget,
            sending.counter.id(),
            Some(posture.clone()),
            &sending.authority,
            sending.tool_calls,
            loaded_view.started_at,
        );
    }
    {
        let mut stream = app.stream.lock().await;
        stream.clear();
        if let Some(header) = header {
            stream.push(header);
        }
    }

    {
        let mut session = app.session.lock().await;
        session.next_turn = (loaded_view.turns.len() as u64) + 1;
        session.current = None;
        session.cancel = None;
        session.held = None;
        session.context = resumed_context;
        session.prefix = PrefixTracker::default();
        // A session whose last job is a proposal comes back *at the gate*.
        // Without this the job sat in `proposed` with nothing holding it, and
        // the next prompt quietly proposed a second one beside it — a question
        // nobody answered, and a second one asked over it.
        session.pending = pending_proposal(&loaded_view);
        // A session resumed **inside an approved job** comes back holding the
        // plan it was approved with, rebuilt against the posture it is being
        // resumed *under* — the same two sentences the reopen route above is
        // written from, and for the same reason: the sandbox is a resolution of
        // the plan, and the plan is what the session keeps.
        //
        // This line was `= None` until 2026-09-21, and that was a widening
        // nothing caught: the live job stayed open across the resume, so the
        // turn's fallback arm answered, and the session came back able to touch
        // everything the policy file grants rather than what the person
        // approved. Item 9's shape one field along — a grant outliving what it
        // was granted under. The floor is what turned it from a quiet widening
        // into a loud narrowing (a resumed job that suddenly cannot write),
        // which is the only reason it was found. See
        // `RECORD/2026-09-21.the-drafts-floor.completed.md` §The second finding.
        //
        // A draft is `None` here exactly as before, and `None` is now the floor
        // rather than the policy file.
        let live = session.context.live_job().and_then(|job| {
            let plan = session.context.job(job).and_then(|job| job.plan.clone())?;
            Some((job, plan))
        });
        session.narrowed = live.and_then(|(job, plan)| {
            let sandbox = plan.narrow(agency.sandbox.as_ref(), job).ok()?;
            Some((job, Arc::new(sandbox)))
        });
    }

    let summary = {
        let mut view = app.view.lock().await;
        let mut live_view = loaded_view.clone();
        live_view.id = LIVE_SESSION.to_string();
        // The fold has one backend and one model, so after a switch it names
        // the one the next turn will use. The *stream* keeps both, in order,
        // which is where "what produced turn 4" is answered.
        live_view.backend = sending.backend.name().to_string();
        live_view.model = sending.model.clone();
        // And the posture only where the stream gained a line carrying it:
        // this view has to equal what folding that stream produces, and two
        // postures that are the same place can still be spelled differently,
        // so overwriting it unconditionally would put a name in the view that
        // no header in the stream ever said.
        if moved {
            live_view.posture = Some(posture);
            // The same line carries these, so they move with it. Where it did
            // not move they already equal what is being sent — that is what
            // `retarget_header` compared to decide — except in the one case it
            // is right to leave alone: a stream from before format 11, which
            // says nothing and whose fold must keep saying nothing.
            live_view.repeat = Some(sending.budget.repeat);
            live_view.prune = Some(sending.budget.prune);
            live_view.results = Some(sending.budget.results);
            live_view.authority_draft = sending.authority.draft_note();
            live_view.authority_plan = sending.authority.plan_note();
            live_view.tool_calls = sending
                .tool_calls
                .native()
                .then(|| sending.tool_calls.as_str().to_string());
        }
        *view = live_view;
        let mut s = view.summary();
        s.id = id.to_string();
        s
    };

    // Written down now rather than at the next turn's checkpoint. A stream
    // line saying *the rest of this was produced somewhere else* is worth
    // nothing if a session that was moved and then left alone loses it — and
    // unlike a new session, this one already has a row, so the checkpoint
    // updates rather than inventing one.
    app.checkpoint().await;

    let hello = ServerMessage::Hello {
        protocol: protocol::VERSION,
        backend: sending.backend.name().to_string(),
        model: sending.model.clone(),
        turn: None,
        session: Some(id.to_string()),
    };
    app.publish(Event::Protocol(hello)).await;

    Json(summary).into_response()
}

/// What a rename asks for. One field, because one field is all a page may
/// change about a session that is not deleting it.
#[derive(serde::Deserialize)]
struct Rename {
    title: String,
}

/// Names a session.
///
/// The first thing the page can *change* about a stored session other than
/// removing it, and deliberately the smallest such thing: a title is a label a
/// person put on a conversation, and nothing downstream reads it. No stream is
/// touched, no fold is rewritten, and the id — which is what everything else
/// addresses a session by — does not move. See
/// `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
///
/// An empty title is refused rather than stored: a session with no name is a
/// row nobody can pick out of the history, and the default (the id) is a better
/// name than nothing.
async fn rename_session(
    State(state): State<AppRouterState>,
    Path(id): Path<String>,
    Json(asked): Json<Rename>,
) -> Response {
    let id = bare(&id);
    let title = asked.title.trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "a session's title cannot be empty").into_response();
    }

    // The live session is the one held in memory, and its fold is what the
    // store is a cache of — so it is renamed there and checkpointed, never
    // written straight into SQLite behind the view that would overwrite it on
    // the next token.
    let active_id = state.app.session_id.lock().await.clone();
    if id == LIVE_SESSION || id == active_id {
        state.app.view.lock().await.title = title.clone();
        state.app.checkpoint().await;
        return Json(serde_json::json!({ "id": active_id, "title": title })).into_response();
    }

    let Some(store_mutex) = &state.app.store else {
        return (StatusCode::NOT_IMPLEMENTED, "session store is disabled").into_response();
    };
    // `retitle` rather than `save`: the title is the one column that is not
    // part of the fold, and rebuilding the row from a view would drop the
    // `provider` beside it. See `SessionStore::retitle`.
    match store_mutex.lock().await.retitle(id, &title) {
        Ok(true) => Json(serde_json::json!({ "id": id, "title": title })).into_response(),
        Ok(false) => not_found("session"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn delete_session_handler(
    State(state): State<AppRouterState>,
    Path(id): Path<String>,
) -> Response {
    let id = bare(&id);
    let active_id = state.app.session_id.lock().await.clone();
    if id == LIVE_SESSION || id == active_id {
        return (StatusCode::BAD_REQUEST, "cannot delete active session").into_response();
    }

    let Some(store_mutex) = &state.app.store else {
        return (StatusCode::NOT_IMPLEMENTED, "session store is disabled").into_response();
    };

    match store_mutex.lock().await.delete(id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found("session"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// The fold for one session: the live one under either of its names, and
/// anything else out of the store.
///
/// The live view is answered from memory rather than from the store even when
/// both have it, because the store is allowed to lag by design and the live
/// fold never is.
async fn view_for(state: &AppRouterState, id: &str) -> Option<SessionView> {
    let id = bare(id);
    let active_id = state.app.session_id.lock().await.clone();
    {
        let view = state.app.view.lock().await;
        if id == view.id || id == active_id {
            return Some(view.clone());
        }
    }
    let store = state.app.store.as_ref()?;
    match store.lock().await.load(id) {
        Ok(found) => found,
        Err(error) => {
            eprintln!("warning: could not read the session store: {error:#}");
            None
        }
    }
}

async fn get_session(Path(id): Path<String>, State(state): State<AppRouterState>) -> Response {
    match view_for(&state, &id).await {
        Some(view) => Json(view).into_response(),
        None => not_found("session"),
    }
}

async fn get_turns(Path(id): Path<String>, State(state): State<AppRouterState>) -> Response {
    match view_for(&state, &id).await {
        Some(view) => Json(view.turns).into_response(),
        None => not_found("session"),
    }
}

/// The turn segment carries the `.json` when it is the last one, so it is
/// stripped before parsing rather than after.
fn parse_turn(turn: &str) -> Option<TurnId> {
    bare(turn).parse().ok()
}

async fn get_turn(
    Path((id, turn)): Path<(String, String)>,
    State(state): State<AppRouterState>,
) -> Response {
    let Some(number) = parse_turn(&turn) else {
        return not_found("turn");
    };
    match view_for(&state, &id)
        .await
        .and_then(|view| view.turn(number).cloned())
    {
        Some(turn) => Json(turn).into_response(),
        None => not_found("turn"),
    }
}

async fn get_prompt(
    Path((id, turn)): Path<(String, String)>,
    State(state): State<AppRouterState>,
) -> Response {
    let Some(number) = parse_turn(&turn) else {
        return not_found("turn");
    };
    let view = view_for(&state, &id).await;
    let found = view.as_ref().and_then(|view| view.turn(number));
    match found {
        Some(turn) => Json(serde_json::json!({
            "turn": turn.turn,
            "text": turn.prompt_sent,
        }))
        .into_response(),
        None => not_found("turn"),
    }
}

/// The budget of the newest turn that has one — the panel asks "what is the
/// context doing now", and a running turn has not reported yet.
async fn get_context(Path(id): Path<String>, State(state): State<AppRouterState>) -> Response {
    let Some(view) = view_for(&state, &id).await else {
        return not_found("session");
    };
    let latest = view.turns.iter().rev().find(|t| t.budget.is_some());
    Json(serde_json::json!({
        "turn": latest.map(|t| t.turn),
        "budget": latest.and_then(|t| t.budget.clone()),
    }))
    .into_response()
}

/// What the session did, added up — the whole of it, not the newest turn.
///
/// The one route here whose answer is over the session rather than over a turn,
/// which is why it is a route of its own rather than a field on the view: the
/// arithmetic is the answer, and a client that wants it does not want the
/// transcript with it. See `RECORD/2026-09-19.one-counting-surface.completed.md`.
async fn get_counts(Path(id): Path<String>, State(state): State<AppRouterState>) -> Response {
    match view_for(&state, &id).await {
        Some(view) => Json(view.counts()).into_response(),
        None => not_found("session"),
    }
}

#[cfg(test)]
mod tests {
    use agent_core::backend::mock::Mock;
    use agent_core::context::{ApproximateCounter, Eviction};
    use agent_core::job::JobState;
    use agent_core::sandbox::SandboxPolicy;

    use super::*;

    const PLAN: &str = "```plan\n{\"objective\":\"add a flag\",\"tasks\":[\"read the CLI\"],\
                        \"files\":[\"Cargo.toml\"],\"commands\":[]}\n```";

    /// The server without a socket in front of it. The handlers are what the
    /// socket calls, one line each, so driving them directly tests the gate
    /// rather than axum.
    fn app(replies: &[&str]) -> Arc<App> {
        app_selecting(replies, 0)
    }

    /// The same server with the selector switched on, which is the only thing
    /// these two builders differ by — a test that changed more than the flag
    /// would not be measuring the flag.
    fn app_selecting(replies: &[&str], select_tokens: u32) -> Arc<App> {
        app_selecting_in(replies, select_tokens, std::env::current_dir().unwrap())
    }

    /// The same, over a tree the test owns and may edit. Nothing here writes
    /// into the checkout: a test that edited this repository to prove a point
    /// about staleness would be the worst possible way to prove it.
    fn app_selecting_in(replies: &[&str], select_tokens: u32, base: PathBuf) -> Arc<App> {
        let agency = Agency::new(
            Arc::new(agent_core::tools::Tools::standard()),
            Arc::new(agent_core::sandbox::Sandbox::new(&SandboxPolicy::default(), &base).unwrap()),
            agent_core::agent::Limits::default().with_max_steps(4),
            None,
        );
        let agency_walk = agency.sandbox.clone();
        Arc::new(App {
            approvers: Approvers::default(),
            walked: Mutex::new(match select_tokens {
                0 => Vec::new(),
                _ => agent_core::repo_map::walk_sources(agency_walk.as_ref()),
            }),
            select_tokens,
            select_weights: Default::default(),
            destination: RwLock::new(Arc::new(Destination {
                backend: Arc::new(
                    Mock::replies(replies.iter().map(|r| (*r).to_string()).collect())
                        .delay(std::time::Duration::ZERO),
                ),
                model: "mock".into(),
                budget: Budget::new(0, 0, Eviction::Turn),
                counter: Arc::new(ApproximateCounter),
                settings: Settings {
                    profile: None,
                    backend: "mock".into(),
                    destination: String::new(),
                    remote: false,
                    model: "mock".into(),
                    window: None,
                    window_from: crate::provider::WindowFrom::Unset,
                    window_caveat: None,
                    reserve: 0,
                    repeat: agent_core::context::Repeat::Once,
                    prune: agent_core::context::Prune::Never,
                    results: agent_core::context::Results::Kept,
                    counter: agent_core::context::Counter::Approximate,
                    counter_warning: None,
                    select_tokens,
                    map_tokens: 0,
                    sandbox: String::new(),
                    base: String::new(),
                    posture: None,
                    floor: None,
                    store: None,
                    unconfigured: true,
                    follows_default: true,
                    tool_calls: crate::provider::ToolCalls::Fenced,
                    authority: crate::provider::AuthorityNotes::default(),
                },
                authority: crate::provider::AuthorityNotes::default(),
                engine: None,
                tool_calls: crate::provider::ToolCalls::Fenced,
            })),
            tokenizer: None,
            session: Mutex::new(Session {
                next_turn: 1,
                current: None,
                cancel: None,
                context: AgentContext::new(SYSTEM).with_tools(agency.definitions()),
                prefix: PrefixTracker::default(),
                pending: None,
                narrowed: None,
                held: None,
            }),
            events: broadcast::channel(1024).0,
            recorder: None,
            agency: RwLock::new(Arc::new(agency)),
            posture: Mutex::new(None),
            // These tests build an `App` directly, and none of them starts a
            // session on a posture: a `None` here is the same answer stdio
            // gets, and the route says so rather than pretending.
            agency_for: None,
            describe_for: None,
            postures: Default::default(),
            postures_path: None,
            icons: std::sync::RwLock::new(Arc::new(crate::icons::Theme::default())),
            engines: Arc::default(),
            tasks: Arc::default(),
            inflight: Arc::default(),
            temperature: None,
            seed: None,
            constraint: None,
            schema_retry: None,
            view: Mutex::new(SessionView::new(LIVE_SESSION, "mock", "mock")),
            session_id: Mutex::new("session-test".into()),
            map_rendered: String::new(),
            // In memory, like everything else these handler tests touch: the
            // store's own behaviour is `tests/store_parity.rs`.
            store: None,
            session_started_at: Mutex::new(0),
            stream: Mutex::new(Vec::new()),
        })
    }

    /// The gap named in
    /// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`'s "the tool-call
    /// probe's own instrument cannot see a retry": before `ModelCall` carried
    /// `retry`, the interceptor's `step > 1` rule silently dropped the one
    /// extra call `SchemaRetry` spends on a drifted reply, because that call
    /// shares `step == 1` with the attempt it retries. This drives a real
    /// retry through `start_turn` and asserts the `StepCall` it should now
    /// produce actually arrives on the event bus.
    #[tokio::test]
    async fn a_schema_retry_reaches_the_step_call_chain_even_at_step_one() {
        let mut app = app_selecting(
            &[
                // Fenced under the tool's own bare name: `score_call` reads
                // this as `Drifted`, not `NoCall` — the one verdict
                // `SchemaRetry` is built to retry.
                "```list_dir\n{}\n```",
                // The retry's reply. Not a call at all, so the turn ends
                // here rather than looping — what is under test is that the
                // retry's own `ModelCall` was traced, not what it answered.
                "there is one file",
            ],
            0,
        );
        Arc::get_mut(&mut app).unwrap().schema_retry = Some(SchemaRetry {
            schema: serde_json::json!({}),
            tool_names: agent_core::tools::Tools::standard().names().collect(),
        });

        let mut events = app.events.subscribe();
        // `start_turn` spawns the turn and returns; it does not run it. The
        // turn itself finishes on its own task, so the assertion waits for
        // the state it left behind rather than for `start_turn` itself.
        start_turn(app.clone(), "list the directory".into()).await;
        assert!(
            until(&app, |s| s.context.turns().len() == 1).await,
            "the turn never finished",
        );

        let mut step_call_at_one = false;
        while let Ok(event) = events.try_recv() {
            if let Event::Trace(TraceMessage::StepCall { step, .. }) = event
                && step == 1
            {
                step_call_at_one = true;
            }
        }
        assert!(
            step_call_at_one,
            "the retry is a second call at step 1 and belongs in the same \
             chain a tool-loop's second call already reaches",
        );
    }

    /// The one line a resume writes, and the two cases it is about.
    ///
    /// The comparison is against the **fold**, which is what the stream's last
    /// header folded to — so a resume that inherits a destination the session
    /// never ran on records that too, which it was doing silently before this
    /// line existed.
    /// The fold of a stream whose last header named this destination's rules.
    ///
    /// A fixture that leaves them `None` is a recording from before format 11,
    /// which is a different case with a different answer — see
    /// `a_resume_records_a_header_where_the_fold_cannot_say_what_it_ran_under`.
    fn rendered_under(view: &mut SessionView, sending: &Destination) {
        view.repeat = Some(sending.budget.repeat);
        view.prune = Some(sending.budget.prune);
        view.results = Some(sending.budget.results);
    }

    /// One destination, for the tests that only care that two of them differ.
    fn mock_destination(backend: &str, model: &str) -> Destination {
        Destination {
            backend: Arc::new(Mock::default()),
            model: model.to_string(),
            budget: Budget::new(8192, 512, Eviction::Turn),
            counter: Arc::new(ApproximateCounter),
            settings: Settings {
                profile: None,
                backend: backend.to_string(),
                destination: String::new(),
                remote: false,
                model: model.to_string(),
                window: Some(8192),
                window_from: crate::provider::WindowFrom::Unset,
                window_caveat: None,
                reserve: 512,
                repeat: agent_core::context::Repeat::Once,
                prune: agent_core::context::Prune::Never,
                results: agent_core::context::Results::Kept,
                counter: agent_core::context::Counter::Approximate,
                counter_warning: None,
                select_tokens: 0,
                map_tokens: 0,
                sandbox: String::new(),
                base: String::new(),
                posture: None,
                floor: None,
                store: None,
                unconfigured: false,
                follows_default: false,
                tool_calls: crate::provider::ToolCalls::Fenced,
                authority: crate::provider::AuthorityNotes::default(),
            },
            authority: crate::provider::AuthorityNotes::default(),
            engine: None,
            tool_calls: crate::provider::ToolCalls::Fenced,
        }
    }

    #[test]
    fn a_resume_records_a_header_only_where_the_destination_moved() {
        let posture = || record::Posture {
            name: None,
            runtime: "host".into(),
            enforcement: "kernel".into(),
            network: false,
        };
        let mut view = SessionView::new("s", "mock", "mock");
        view.started_at = 1_700_000_000_000;
        // The posture the stream's last header named. It is a term of the
        // comparison and not a passenger — see the test below.
        view.posture = Some(posture());
        // And the fourth term, which the mock destination is under. A fold that
        // does not carry them is a stream from before format 11, and that case
        // has a test of its own below.
        rendered_under(&mut view, &mock_destination("mock", "mock"));

        // The same destination and the same posture the stream already names:
        // nothing to say.
        assert!(
            retarget_header(
                &view,
                &mock_destination("mock", "mock"),
                agent_core::context::Counter::Approximate,
                posture(),
            )
            .is_none(),
            "a resume that changed nothing must not write a line saying it did",
        );

        // A different model on the same backend is a different destination —
        // the numbers a header exists to make comparable are the model's.
        let moved = retarget_header(
            &view,
            &mock_destination("mock", "other"),
            agent_core::context::Counter::Approximate,
            posture(),
        )
        .expect("the destination moved, so the stream has to say so");
        match moved {
            record::RecordLine::Header {
                model, started_at, ..
            } => {
                assert_eq!(model, "other");
                assert_eq!(
                    started_at, view.started_at,
                    "every at_ms in the stream is relative to the session's own start, \
                     so a second header carries it rather than the moment of the resume",
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// The case that wrote nothing, which is the one that mattered.
    ///
    /// A resume from a container onto a host on the same model moved nothing
    /// `retarget_header` compared, so the stream gained no line and the rest of
    /// the session read as though it had stayed contained. `resume_session`
    /// refuses that now; this pins the recording's half of it, because a
    /// recording's honesty should not depend on an access check.
    ///
    /// See `RECORD/2026-09-17.what-an-approval-was-granted-under.completed.md`.
    #[test]
    fn a_resume_records_a_header_where_only_the_posture_moved() {
        let sending = mock_destination("mock", "mock");
        let contained = record::Posture {
            name: Some("container".into()),
            runtime: "docker (luu-worker:dev)".into(),
            enforcement: "kernel".into(),
            network: false,
        };
        let host = record::Posture {
            name: None,
            runtime: "host".into(),
            enforcement: "kernel".into(),
            network: false,
        };

        let mut view = SessionView::new("s", "mock", "mock");
        view.started_at = 1_700_000_000_000;
        view.posture = Some(contained.clone());
        rendered_under(&mut view, &sending);

        let moved = retarget_header(
            &view,
            &sending,
            agent_core::context::Counter::Approximate,
            host.clone(),
        )
        .expect("the posture moved, so the stream has to say so");
        match moved {
            record::RecordLine::Header { posture, model, .. } => {
                assert_eq!(model, "mock", "the destination did not move");
                assert_eq!(
                    posture.expect("a header a resume writes always names one"),
                    host,
                    "the line says where the rest of these turns ran, not where they started",
                );
            }
            other => panic!("{other:?}"),
        }

        // And the name alone is not the comparison: the same three facts under
        // two spellings is the same place, and a line saying otherwise would be
        // recording an editor's habit as a move.
        let renamed = record::Posture {
            name: Some("contained".into()),
            ..contained.clone()
        };
        assert!(
            retarget_header(
                &view,
                &sending,
                agent_core::context::Counter::Approximate,
                renamed,
            )
            .is_none(),
            "a posture compared on its name would write a line every time somebody \
             renamed one in config.toml",
        );
    }

    /// A session recorded before format 10 has no posture, and *unknown* is not
    /// `host`: the resume is allowed through — refusing it would make a history
    /// unreadable on the strength of a guess — and the stream gains the line
    /// that says where the rest of it ran.
    #[test]
    fn a_session_with_no_recorded_posture_gains_a_header_rather_than_a_refusal() {
        let sending = mock_destination("mock", "mock");
        let mut view = SessionView::new("s", "mock", "mock");
        view.started_at = 1_700_000_000_000;
        assert!(view.posture.is_none(), "what a pre-format-10 fold produces");

        assert!(
            retarget_header(
                &view,
                &sending,
                agent_core::context::Counter::Approximate,
                record::Posture {
                    name: None,
                    runtime: "host".into(),
                    enforcement: "kernel".into(),
                    network: false,
                },
            )
            .is_some(),
            "an unknown posture is the one case where the header is the only \
             thing that will ever say where these turns ran",
        );
    }

    /// The fourth term's own case: a resume where nothing moved but the rules
    /// the window is rendered under.
    ///
    /// It is the posture's bug one field along. A resume may change these —
    /// nothing was approved under them, which is why they are not refused — so
    /// a resume that changes them and writes no line leaves the rest of the
    /// stream reading as though it had stayed under the old ones. That is what
    /// `a_resume_records_a_header_where_only_the_posture_moved` was written
    /// about, and the reason this one exists the same day the rules became a
    /// session's to choose.
    #[test]
    fn a_resume_records_a_header_where_only_the_resend_rules_moved() {
        let host = record::Posture {
            name: None,
            runtime: "host".into(),
            enforcement: "kernel".into(),
            network: false,
        };
        let sending = mock_destination("mock", "mock");
        let mut view = SessionView::new("s", "mock", "mock");
        view.started_at = 1_700_000_000_000;
        view.posture = Some(host.clone());
        rendered_under(&mut view, &sending);

        // The same destination and the same posture, with rule A turned off —
        // which is the direction that is a *move* since the default flipped,
        // and the whole of what this test needs from the rule.
        let always = Destination {
            budget: Budget {
                repeat: agent_core::context::Repeat::Always,
                ..sending.budget
            },
            ..mock_destination("mock", "mock")
        };
        let moved = retarget_header(
            &view,
            &always,
            agent_core::context::Counter::Approximate,
            host.clone(),
        )
        .expect("the rules moved, so the stream has to say so");
        match moved {
            record::RecordLine::Header {
                model,
                repeat,
                prune,
                results,
                ..
            } => {
                assert_eq!(model, "mock", "the destination did not move");
                assert_eq!(
                    (repeat, prune, results),
                    (
                        Some(agent_core::context::Repeat::Always),
                        Some(agent_core::context::Prune::Never),
                        Some(agent_core::context::Results::Kept)
                    ),
                    "the line says what the rest of these turns were rendered under",
                );
            }
            other => panic!("{other:?}"),
        }

        // And each of the other two on its own, because a comparison that read
        // only the first field would pass everything above.
        for under in [
            Budget {
                prune: agent_core::context::Prune::Behind,
                ..sending.budget
            },
            Budget {
                results: agent_core::context::Results::Cited,
                ..sending.budget
            },
        ] {
            assert!(
                retarget_header(
                    &view,
                    &Destination {
                        budget: under,
                        ..mock_destination("mock", "mock")
                    },
                    agent_core::context::Counter::Approximate,
                    host.clone(),
                )
                .is_some(),
                "a rule that moved without a line is a recording describing two \
                 windows under one header: {under:?}",
            );
        }
    }

    /// A fold from before format 11 cannot say what its turns were rendered
    /// under, and *unknown* is not *the defaults*.
    ///
    /// Same answer as the posture's unknown one field along, and for the same
    /// reason: the resume is allowed through, and the line is written because
    /// it is the only thing that will ever say what the rest of the stream ran
    /// under.
    #[test]
    fn a_resume_records_a_header_where_the_fold_cannot_say_what_it_ran_under() {
        let host = record::Posture {
            name: None,
            runtime: "host".into(),
            enforcement: "kernel".into(),
            network: false,
        };
        let sending = mock_destination("mock", "mock");
        let mut view = SessionView::new("s", "mock", "mock");
        view.started_at = 1_700_000_000_000;
        view.posture = Some(host.clone());
        assert_eq!(
            (view.repeat, view.prune, view.results),
            (None, None, None),
            "what a pre-format-11 fold produces",
        );

        assert!(
            retarget_header(
                &view,
                &sending,
                agent_core::context::Counter::Approximate,
                host,
            )
            .is_some(),
            "a fold that says nothing cannot be *the same as* the defaults: reading \
             them back for it would put a claim in a record that never made one",
        );
    }

    /// §fourth, as code: what a saved table may do to a running conversation.
    ///
    /// The three rules are not one case, and the asymmetry is not about the
    /// rules being different sizes — it is about which of them store state. A
    /// control that reports a change it did not make is the cheap failure here;
    /// the expensive one is rule C, where handing output back can spend the
    /// conversation through the floor.
    /// What a browser on this machine sends as `Host`, and what a rebinding
    /// page on another domain does.
    #[test]
    fn only_a_loopback_host_may_start_an_engine() {
        for host in [
            "127.0.0.1:7878",
            "localhost:7878",
            "LOCALHOST",
            "[::1]:7878",
            "127.0.0.2",
        ] {
            assert!(host_is_loopback(host), "{host}");
        }
        for host in [
            "evil.example:7878",
            "192.168.1.40:7878",
            "",
            "localhost.evil.example",
        ] {
            assert!(!host_is_loopback(host), "{host}");
        }
    }

    #[test]
    fn a_saved_table_moves_a_running_session_only_where_moving_it_is_free() {
        use agent_core::context::{Prune, Repeat, Results};

        // By name for rule A, because `Budget::new` turns it on since
        // 2026-09-19 and a variable called `off` that is not off would make
        // both directions below read as the same case.
        let off = Budget::new(8192, 512, Eviction::Turn).repeating(Repeat::Always);
        let on = Budget {
            repeat: Repeat::Once,
            prune: Prune::Behind,
            results: Results::Cited,
            ..off
        };
        let asked = |repeat, prune, results| crate::provider::Resend {
            repeat,
            prune,
            results,
        };

        // Everything turned on: all three move, nothing waits. Turning a rule
        // on is clean for all three — the line starts moving, and rule A is
        // chosen inside the window being rendered.
        let (moved, waiting) = resend_live(
            asked(
                Some(Repeat::Once),
                Some(Prune::Behind),
                Some(Results::Cited),
            ),
            off,
        );
        assert_eq!(
            (moved.repeat, moved.prune, moved.results),
            (Repeat::Once, Prune::Behind, Results::Cited),
        );
        assert!(waiting.is_empty(), "{waiting:?}");

        // Everything turned off, from a session that is under all three. Rule A
        // moves; B and C do not, and each says why.
        let (moved, waiting) = resend_live(
            asked(
                Some(Repeat::Always),
                Some(Prune::Never),
                Some(Results::Kept),
            ),
            on,
        );
        assert_eq!(
            moved.repeat,
            Repeat::Always,
            "rule A stores nothing, so it is clean in both directions",
        );
        assert_eq!(
            (moved.prune, moved.results),
            (Prune::Behind, Results::Cited),
            "the ratchet does not retreat and the floor does not give turns back",
        );
        assert_eq!(
            waiting.len(),
            2,
            "both say why they did not move: {waiting:?}"
        );
        assert!(waiting.iter().any(|line| line.starts_with("prune:")));
        assert!(waiting.iter().any(|line| line.starts_with("results:")));

        // Turning off what is already off is not a refusal. Nothing moved and
        // nothing waits, because there is nothing a ratchet could fail to give
        // back — a control that warned here would be warning about a no-op.
        let (moved, waiting) =
            resend_live(asked(None, Some(Prune::Never), Some(Results::Kept)), off);
        assert_eq!(moved, off);
        assert!(waiting.is_empty(), "{waiting:?}");

        // A key the table does not name leaves the live session where it is.
        // The file is the machine's default and may say nothing about a rule.
        let (moved, waiting) = resend_live(asked(None, None, None), on);
        assert_eq!(moved, on);
        assert!(waiting.is_empty(), "{waiting:?}");

        // Rule C is three values now, and the rung between them is not free
        // either: `Cited` to `CitedReads` hands every command's output back,
        // which is the biggest single thing the rule was holding out of the
        // prompt. A check written as `== Kept` would have let it straight
        // through.
        let (moved, waiting) = resend_live(asked(None, None, Some(Results::CitedReads)), on);
        assert_eq!(
            moved.results,
            Results::Cited,
            "a partial step down is still a step down",
        );
        assert_eq!(waiting.len(), 1, "{waiting:?}");
        assert!(waiting[0].contains("cited_reads"), "{waiting:?}");

        // And the way up is free, from either rung.
        let reads = Budget {
            results: Results::CitedReads,
            ..off
        };
        let (moved, waiting) = resend_live(asked(None, None, Some(Results::Cited)), reads);
        assert_eq!(moved.results, Results::Cited);
        assert!(waiting.is_empty(), "{waiting:?}");
        let (moved, waiting) = resend_live(asked(None, None, Some(Results::CitedReads)), off);
        assert_eq!(moved.results, Results::CitedReads);
        assert!(waiting.is_empty(), "{waiting:?}");
    }

    /// A directory that removes itself, so a failing test does not leave one
    /// behind and the next run does not read it.
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "luu-{name}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("a clock after 1970")
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&path).expect("the temporary directory");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The handlers spawn, so the assertions wait for the state they are about.
    async fn until(app: &Arc<App>, what: impl Fn(&Session) -> bool) -> bool {
        for _ in 0..200 {
            if what(&*app.session.lock().await) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    /// A helper for the tests below: the gate's approval, which names no job
    /// and whose one `Option` is the id it is about to hand out.
    async fn approve(app: &Arc<App>) {
        approve_plan(
            app.clone(),
            None,
            vec![],
            vec![],
            vec![],
            None,
            None,
            None,
            None,
            None,
        )
        .await;
    }

    /// The change a person can observe, in one test: a prompt with nothing open
    /// **runs**, in the draft it opens, instead of buying a planning call and
    /// waiting behind the gate.
    #[tokio::test]
    async fn a_prompt_with_nothing_open_runs_in_the_draft_it_opens() {
        let app = app(&["looked around"]);
        on_prompt(app.clone(), "have a look".into()).await;
        assert!(
            until(&app, |s| s.context.turns().len() == 1).await,
            "the prompt never ran",
        );

        let session = app.session.lock().await;
        assert!(session.pending.is_none(), "nothing was proposed");
        let job = session.context.job(1).expect("a draft opened");
        assert!(job.is_draft(), "its objective is known and its plan is not");
        assert_eq!(job.objective, "have a look");
        assert_eq!(
            session.context.turns()[0].job,
            1,
            "the turn belongs to the draft it opened",
        );
    }

    /// The model's own door. An ordinary draft turn whose answer carries a plan
    /// block is a suggestion, and it reaches the person as a proposal — nothing
    /// is approved by it and nothing runs under it.
    #[tokio::test]
    async fn a_plan_the_model_suggests_reaches_the_gate() {
        let app = app(&[PLAN]);
        on_prompt(app.clone(), "add a flag".into()).await;
        assert!(until(&app, |s| s.pending.is_some()).await, "no proposal");

        let session = app.session.lock().await;
        let pending = session.pending.as_ref().expect("a plan on the table");
        assert_eq!(pending.plan.files, ["Cargo.toml"]);
        assert_eq!(pending.source, PlanSource::Model);
        assert_eq!(
            session.context.jobs().len(),
            1,
            "a suggestion is not a job: the draft is the only one",
        );
        assert!(session.context.job(1).unwrap().is_draft());
    }

    /// The explicit door, and it plans over the draft rather than over a held
    /// prompt — the planning call is not remembered, so the draft's turn count
    /// does not move.
    #[tokio::test]
    async fn request_plan_asks_for_one_over_the_draft() {
        let app = app(&["looked around", PLAN]);
        on_prompt(app.clone(), "have a look".into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 1).await);

        request_plan(app.clone()).await;
        assert!(until(&app, |s| s.pending.is_some()).await, "no proposal");

        let session = app.session.lock().await;
        assert_eq!(
            session.context.turns().len(),
            1,
            "the planning call is the one turn that is not remembered",
        );
        assert_eq!(session.pending.as_ref().unwrap().plan.files, ["Cargo.toml"]);
    }

    /// Approving *is* closing: the draft folds and the job the plan describes
    /// opens, in one action and with the id the approval hands out.
    #[tokio::test]
    async fn approving_closes_the_draft_and_opens_the_job() {
        let app = app(&[PLAN, "the answer"]);
        on_prompt(app.clone(), "add a flag".into()).await;
        assert!(until(&app, |s| s.pending.is_some()).await);

        approve(&app).await;
        assert!(until(&app, |s| s.context.jobs().len() == 2).await, "no job");

        let session = app.session.lock().await;
        assert!(session.pending.is_none());
        let draft = session.context.job(1).unwrap();
        assert!(draft.is_closed(), "approving closed the draft");
        assert!(
            draft
                .summary
                .as_ref()
                .unwrap()
                .text
                .starts_with("[draft closed]"),
            "and a draft's fold says which half of the alternation it is",
        );
        let job = session.context.job(2).unwrap();
        assert!(!job.is_draft(), "the job an approval opens holds the plan");
        assert_eq!(job.plan.as_ref().unwrap().files, ["Cargo.toml"]);
        assert_eq!(
            session.context.current_job(),
            Some(2),
            "and the work is what is live now",
        );
        assert!(
            session.narrowed.as_ref().is_some_and(|(id, _)| *id == 2),
            "the sandbox narrows at the approval, and only there",
        );
    }

    /// Declining ends nothing. The prompt this used to drop on the floor does
    /// not exist any more — the conversation simply continues in the draft.
    #[tokio::test]
    async fn declining_keeps_the_draft_open_and_keeps_its_turns() {
        let app = app(&[PLAN, "still looking"]);
        on_prompt(app.clone(), "add a flag".into()).await;
        assert!(until(&app, |s| s.pending.is_some()).await);

        decline_plan(app.clone()).await;
        {
            let session = app.session.lock().await;
            assert!(session.pending.is_none());
            assert_eq!(session.context.jobs().len(), 1, "a refusal opens nothing");
            assert_eq!(
                session.context.current_job(),
                Some(1),
                "declining keeps you in the job you are in",
            );
            assert_eq!(session.context.rounds().len(), 1, "the round is kept");
        }

        on_prompt(app.clone(), "what about this".into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 2).await);
        let session = app.session.lock().await;
        assert_eq!(
            session.context.turns()[1].job,
            1,
            "the conversation continues in the same draft",
        );
    }

    /// *More changes* said out loud: a prompt arriving while a plan is on the
    /// table is the same answer as decline, and it runs.
    #[tokio::test]
    async fn a_prompt_while_a_plan_is_on_the_table_is_more_changes() {
        let app = app(&[PLAN, "adjusted"]);
        on_prompt(app.clone(), "add a flag".into()).await;
        assert!(until(&app, |s| s.pending.is_some()).await);

        on_prompt(app.clone(), "make it two flags".into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 2).await);

        let session = app.session.lock().await;
        assert!(session.pending.is_none(), "the plan came off the table");
        assert_eq!(
            session.context.rounds().len(),
            1,
            "and it was answered rather than forgotten",
        );
        assert_eq!(session.context.jobs().len(), 1, "still the one draft");
    }

    #[tokio::test]
    async fn a_prompt_inside_a_live_job_is_a_turn_and_not_another_gate() {
        let app = app(&[PLAN, "the answer", "and the tests"]);
        on_prompt(app.clone(), "add a flag".into()).await;
        assert!(until(&app, |s| s.pending.is_some()).await);
        approve(&app).await;
        assert!(until(&app, |s| s.context.jobs().len() == 2).await);

        on_prompt(app.clone(), "now the tests".into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 2).await);

        let session = app.session.lock().await;
        assert_eq!(session.context.jobs().len(), 2, "no third job");
        assert!(session.pending.is_none(), "and no second gate");
        assert_eq!(
            session.context.turns()[1].job,
            2,
            "it is work, not drafting"
        );
    }

    #[tokio::test]
    async fn closing_folds_the_job_and_reopening_unfolds_it() {
        let app = app(&[PLAN, "the answer"]);
        on_prompt(app.clone(), "add a flag".into()).await;
        assert!(until(&app, |s| s.pending.is_some()).await);
        approve(&app).await;
        assert!(until(&app, |s| s.context.jobs().len() == 2).await);
        on_prompt(app.clone(), "do it".into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 2).await);

        close_job(app.clone(), 2).await;
        {
            let session = app.session.lock().await;
            let job = session.context.job(2).unwrap();
            assert_eq!(job.state, JobState::Closed);
            assert!(job.summary.as_ref().unwrap().text.contains("add a flag"));
            assert_eq!(
                session.context.turns().len(),
                2,
                "closing is an event: the turns are still there",
            );
        }

        reopen_job(app.clone(), 2).await;
        let session = app.session.lock().await;
        assert_eq!(session.context.job(2).unwrap().state, JobState::Open);
        assert!(session.context.job(2).unwrap().summary.is_none());
    }

    /// The test that would have caught the stale walk, at the seam it was on.
    ///
    /// `repo_map`'s own tests cover `rewalk_sources`; this one covers the thing
    /// that was actually broken, which is that nothing in `serve` called
    /// anything like it. Two turns of one session with the file changing in
    /// between: turn one sees the definition, the file moves under the session,
    /// turn two must still see the definition and not the bytes that are now at
    /// its old line numbers.
    ///
    /// The edit is made by the test rather than by a tool call, and that is the
    /// honest shape: what is under test is the seam between the walk and the
    /// filesystem, not `edit_file`. A turn that wrote the file through the tool
    /// would exercise more and prove the same thing less directly.
    #[tokio::test]
    async fn a_second_turn_selects_the_file_as_it_is_now() {
        // Two definitions, so the first one's span *ends* before the second
        // rather than running to the end of the file. A span that ran to EOF
        // would still contain the definition after everything shifted down,
        // and the test would pass against the bug it exists to catch.
        const BEACON: &str = "//! The beacon, which exists to be found.\n\
                              \n\
                              pub fn find_the_beacon() {\n\
                                  let beacon = 1;\n\
                              }\n\
                              \n\
                              pub fn something_else() {\n\
                                  let other = 2;\n\
                              }\n";
        const PLAN_FOR_BEACON: &str = "```plan\n{\"objective\":\"find it\",\
                                       \"tasks\":[\"read it\"],\
                                       \"files\":[\"src/beacon.rs\"],\
                                       \"commands\":[]}\n```";
        const ASK: &str = "find_the_beacon";

        let dir = TempRepo::new("serve-rewalk");
        std::fs::create_dir_all(dir.path().join("src")).expect("the source directory");
        std::fs::write(dir.path().join("src/beacon.rs"), BEACON).expect("beacon.rs");

        let app = app_selecting_in(
            &[PLAN_FOR_BEACON, "the answer", "the answer again"],
            2048,
            dir.path().to_path_buf(),
        );

        on_prompt(app.clone(), ASK.into()).await;
        assert!(until(&app, |s| s.pending.is_some()).await);
        approve(&app).await;
        assert!(until(&app, |s| s.context.jobs().len() == 2).await);
        // The drafting turn ran before the gate; the turn the plan confines is
        // the one after it.
        on_prompt(app.clone(), ASK.into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 2).await);

        let held = |session: &Session, turn: usize| -> String {
            session.context.turns()[turn]
                .code_context
                .iter()
                .map(|fragment| fragment.text.clone())
                .collect()
        };
        assert!(
            held(&*app.session.lock().await, 0).contains("pub fn find_the_beacon"),
            "the first turn has to hold it, or the second turn proves nothing",
        );

        // Thirty lines above everything — more than the definition's own span,
        // so the old range cannot overlap the new position by accident.
        std::fs::write(
            dir.path().join("src/beacon.rs"),
            format!("{}{BEACON}", "// pushed down\n".repeat(30)),
        )
        .expect("the edit");

        on_prompt(app.clone(), ASK.into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 2).await);

        let session = app.session.lock().await;
        let second = held(&session, 1);
        assert!(
            second.contains("pub fn find_the_beacon"),
            "the second turn was handed the file's old line numbers: {second:?}",
        );
        assert!(
            !second.contains("// pushed down\n// pushed down"),
            "and it was handed the definition rather than the padding that took \
             its place: {second:?}",
        );
    }

    /// The reason selection is allowed in `serve` at all.
    ///
    /// The gate exists so a person decides what a job may touch. A selector
    /// that read outside the approved plan would put files into the prompt
    /// that the person refused — quietly, because nobody typed their names.
    /// So the selection is read through `session.narrowed`, and this is the
    /// test that says the narrowing is the one doing the holding.
    #[tokio::test]
    async fn a_selection_cannot_reach_outside_the_approved_plan() {
        const ONE_FILE: &str = "```plan\n{\"objective\":\"read one file\",\
                                \"tasks\":[\"read it\"],\"files\":[\"src/serve.rs\"],\
                                \"commands\":[]}\n```";
        let app = app_selecting(&[ONE_FILE, "the answer"], 2048);
        let walked = app.walked.lock().await.clone();
        assert!(
            !walked.is_empty(),
            "the walk found nothing, so this test would pass by holding zero files",
        );

        // Without the plan this query reaches several files, which is what
        // makes the assertion below about the narrowing rather than about a
        // query that was only ever going to pick one thing.
        const ASK: &str = "the session store, the http server and the exporter";
        let unnarrowed = agent_core::select::select(
            &walked,
            app.agency().await.sandbox.as_ref(),
            ASK,
            2048,
            app.destination().await.counter.as_ref(),
            &app.select_weights,
        );
        let reach: std::collections::HashSet<_> = unnarrowed
            .specs()
            .iter()
            .map(|spec| spec.path.clone())
            .collect();
        assert!(
            reach.len() > 1,
            "the unnarrowed selection reached only {reach:?}, so confining it proves nothing",
        );

        on_prompt(app.clone(), ASK.into()).await;
        assert!(until(&app, |s| s.pending.is_some()).await);
        approve(&app).await;
        assert!(until(&app, |s| s.context.jobs().len() == 2).await);
        // The drafting turn ran before the gate; the turn the plan confines is
        // the one after it.
        on_prompt(app.clone(), ASK.into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 2).await);

        let session = app.session.lock().await;
        let code = &session.context.turns()[1].code_context;
        assert!(
            !code.is_empty(),
            "the selector chose nothing, so the confinement below is vacuous",
        );
        for fragment in code {
            // The path carries its span — `src/serve.rs:1-25` — and the grant
            // is about the file.
            let file = fragment.path.split(':').next().unwrap_or(&fragment.path);
            assert!(
                file.ends_with("src/serve.rs"),
                "the plan granted src/serve.rs and the selection reached {} anyway",
                fragment.path,
            );
        }
    }

    /// A small model answering in prose is the ordinary case and it must not
    /// cost the gate. What it cannot do any more is open one by itself: prose
    /// in a draft turn is an answer, and the gate is opened by asking.
    #[tokio::test]
    async fn a_model_that_answers_in_prose_still_gets_a_gate() {
        let app = app(&["I'll read the CLI and add it.", "still prose, I'm afraid"]);
        on_prompt(app.clone(), "add a flag".into()).await;
        assert!(until(&app, |s| s.context.turns().len() == 1).await);
        assert!(
            app.session.lock().await.pending.is_none(),
            "prose is not a suggestion: there is no plan block in it to put up",
        );

        request_plan(app.clone()).await;
        assert!(until(&app, |s| s.pending.is_some()).await, "no proposal");

        let session = app.session.lock().await;
        let pending = session.pending.as_ref().expect("a plan on the table");
        assert_eq!(
            pending.objective, "add a flag",
            "the draft's own objective stands in when the model declares nothing",
        );
        assert!(pending.plan.files.is_empty());
        assert_eq!(
            pending.source,
            PlanSource::Prose,
            "and which of the two happened travels with the proposal",
        );
    }
}
