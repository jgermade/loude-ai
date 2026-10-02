//! `luu` — the CLI.
//!
//! One name, one binary: `src/bin/luu.rs` is a thin wrapper over [`run`]. The
//! second name the design used to install alongside it is gone; see
//! `RECORD/2026-09-04.the-name-and-the-config-dir.completed.md`.

use std::time::Duration;

use agent_core::agent::{DEFAULT_MAX_STEPS, SchemaRetry, run_agent_turn};
use agent_core::api::{Counts, SessionView, Tokens};
use agent_core::approval::{Approval, Approvers, Signer};
use agent_core::backend::{
    Backend, CompletionRequest, Constraint, mock::Mock, ollama::Ollama, openai::OpenAi,
};
use agent_core::context::{
    Budget, Context as AgentContext, Counter, Eviction, Fragment, Prune, Repeat, Results,
};
use agent_core::fragment;
use agent_core::protocol::{self, ClientMessage as ServerBoundMessage, ServerMessage};
use agent_core::repo_map::{Order, RepoMap};
use agent_core::sandbox::{Access, Enforcement, Sandbox, SandboxPolicy};
use agent_core::task::{ApprovedBy, ClosedBy, Plan, PlanSource};
use agent_core::tools::Tools;
use agent_core::trace::TraceMessage;
use agent_core::turn::{EndReason, TurnEvent};
use agent_core::worker::{Runtime, Worker, WorkerConfig, WorkerSpec, serve_stdio};

use crate::provider::BackendKind;
use crate::session::{
    Agency, DEFAULT_RESERVE, Event, PrefixTracker, Recorder, SYSTEM, counter_for, now_ms, rendered,
};
use anyhow::{Context, Result};

pub mod auth;
pub mod config;
pub mod engines;
pub mod export;
pub mod highlight;
pub mod hosts;
pub mod icons;
pub mod log;
pub mod models;
pub mod provider;
pub mod runtimes;
pub mod secret;
pub mod serve;
pub mod session;
pub mod store;
#[cfg(unix)]
pub mod terminal;
pub mod workspace;
use clap::{Parser, Subcommand, ValueEnum};
use tokio::io::{AsyncWriteExt, stdout};
use tokio::sync::{mpsc, watch};

#[derive(Parser)]
#[command(
    name = "luu",
    version,
    about = "Local AI agent, built for small models"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Turn recordings into the static twin of the read API.
    ///
    /// The same JSON the live server serves, as files a static host can
    /// answer with — which is what makes the GitHub Pages deploy more than
    /// a screenshot.
    Export {
        /// Recorded `.jsonl` sessions. Each file's stem becomes its session id.
        #[arg(required = true)]
        records: Vec<std::path::PathBuf>,

        /// Where to write the tree (`sessions.json`, `sessions/<id>/…`).
        #[arg(long, short)]
        out: std::path::PathBuf,

        /// How the page reaches the recordings, relative to the site root.
        #[arg(long, default_value = "./fixtures")]
        record_base: String,
    },

    /// Add a recording up: what its window rules, its gate and its tools did
    /// over the whole session, rather than per render.
    ///
    /// The family `luu tools`, `luu map` and `luu select` are in — print what a
    /// thing resolves to, so it can be looked at rather than inferred. This is
    /// the one that replaces a `grep` for a trace line and a `wc -l`, which is
    /// how `RECORD/runs/2026-09-19.edit-reread-mock` got three of its four
    /// columns. See `RECORD/2026-09-19.one-counting-surface.completed.md`.
    Count {
        /// A recorded `.jsonl` session.
        record: std::path::PathBuf,

        /// The whole tally as JSON, for a script rather than a reader.
        #[arg(long)]
        json: bool,
    },

    /// The executor half of the worker IPC: read tool calls on stdin, run them,
    /// write outcomes on stdout.
    ///
    /// This is what runs *inside* the container — the container's only process,
    /// so its lifetime is the session's and `--rm` plus a closed stdin is the
    /// whole of the cleanup. It takes no sandbox flags on purpose: the sandbox
    /// arrives with every call, as the policy it is built from, because the
    /// paths a host resolved are not the paths an image has. See
    /// `RECORD/2026-09-02.the-worker-and-the-seam.completed.md`.
    Worker {
        /// A command the policy allows, so the handshake can report whether
        /// this side's `PATH` actually has it. Repeatable.
        ///
        /// It is the third failure mode — granted by the policy, absent from
        /// the image — answered by the only process that can see the image.
        #[arg(long = "command", value_name = "NAME")]
        commands: Vec<String>,
    },

    /// Print the resolved sandbox and the exact tool definitions that go into
    /// the prompt.
    ///
    /// The definitions are the second half of the cached prefix, so being able
    /// to look at the bytes is the difference between a cache miss you can see
    /// and one you cannot.
    Tools {
        #[command(flatten)]
        sandbox: SandboxArgs,
    },

    /// Approval keys: make one, or sign an approval with it.
    ///
    /// A verification path with no way to produce a signature is a feature
    /// nobody can run, and the signing half belongs where a remote operator's
    /// `luu` can call it rather than inside one surface's UI. See
    /// `RECORD/2026-09-04.signed-approvals.completed.md`.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },

    /// Print the repository map that a budget resolves to, and what it cost.
    ///
    /// The map is the last block of the cached prefix, so being able to look at
    /// the bytes is the same difference `luu tools` exists for — and it is the
    /// only place the walk's two rules (no dot-directories, no `target`) are
    /// visible rather than inferred from a number that looks wrong.
    /// What one prompt points at: the fragments a turn would be given, and why.
    ///
    /// The map's counterpart, and the opposite kind of block — the map is the
    /// same for every turn and is cached; a selection is this turn's alone. See
    /// `RECORD/2026-09-05.choosing-fragments.completed.md`.
    Select {
        /// The prompt to select against.
        query: String,

        #[command(flatten)]
        sandbox: SandboxArgs,

        /// Tokens to spend on fragments.
        #[arg(long, default_value_t = 1024)]
        select_tokens: u32,

        /// Score a file by what its own module doc says, as well as by what it
        /// defines. On: measured at 32 of 38 targets held against 19 without
        /// it, on a tree that writes its `//!` blocks.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        select_docs: bool,

        /// Also score a file for being one hop from a file that matched. Off,
        /// and the reason is a measurement: the hop moves the right file out of
        /// first place 5 times in 38 and holds no more targets.
        #[arg(long)]
        select_graph: bool,

        /// The model's `tokenizer.json`. Without it the count is `chars/4`.
        #[arg(long)]
        tokenizer: Option<std::path::PathBuf>,

        /// Print every file that scored and why, not only the ones that fitted.
        #[arg(long)]
        explain: bool,
    },

    Map {
        #[command(flatten)]
        sandbox: SandboxArgs,

        /// Tokens to spend on the outline.
        #[arg(long, default_value_t = 1024)]
        map_tokens: u32,

        /// Order the map by what the rest of the tree depends on, instead of
        /// by path. Off, and the reason is a measurement rather than caution:
        /// at 1024 tokens the alphabet holds five files and the ranking holds
        /// two, because rank order puts the big central files first and the
        /// fill rule stops at the first that does not fit. `luu map --explain`
        /// prints the ranking either way. See
        /// `RECORD/2026-09-02.ranking-the-map.completed.md`.
        #[arg(long)]
        map_rank: bool,

        /// Order the map by direct weighted in-degree (breadth of inbound references).
        #[arg(long)]
        map_in_degree: bool,

        /// Pack the token budget non-greedily (skip oversized files to fit smaller ones).
        #[arg(long)]
        map_non_greedy: bool,

        /// The model's `tokenizer.json`. Without it the count is `chars/4` and
        /// says so, which is a different number from what a run would spend.
        #[arg(long)]
        tokenizer: Option<std::path::PathBuf>,

        /// Print the ranking that chose the order: every outlined file, its
        /// score, and the files that reference it.
        ///
        /// The order stopped being the alphabet's, so it stopped being legible
        /// from `ls` — and a selection nobody can interrogate is the thing
        /// embeddings were rejected for. It prints *after* the block and never
        /// inside it: a map that explained itself to the model would spend the
        /// budget on its own footnotes.
        #[arg(long)]
        explain: bool,
    },

    /// Serve the debug UI and the agent protocol over HTTP.
    Serve {
        #[command(flatten)]
        sandbox_args: SandboxArgs,

        #[arg(long, default_value = "127.0.0.1:7878")]
        bind: std::net::SocketAddr,

        /// A file holding the bearer token every `/ws` and `/api/*` request
        /// must carry. Required to bind anything but a loopback address:
        /// `/ws` approves plans, and off loopback that is one request away
        /// from anyone who can reach the port.
        #[arg(long, value_name = "PATH")]
        auth_token_file: Option<std::path::PathBuf>,

        /// Where sessions are cached between restarts, as SQLite. Defaults to
        /// `sessions.db` in the state directory the first run chose
        /// (`~/.luu` or `~/.config/luu`, or wherever `LUU_HOME` names);
        /// `--no-store` keeps the session in memory,
        /// which is what `serve` did before the store existed.
        ///
        /// Deliberately not beside `luu.toml`: the policy file describes *this
        /// project* and is meant to be committed with it, and a session store
        /// that travelled with a checkout would put one project's conversation
        /// into every clone of it. See
        /// `RECORD/2026-09-02.sessions-in-sqlite.completed.md`.
        #[arg(long, value_name = "PATH")]
        store: Option<std::path::PathBuf>,

        /// Keep the session in memory only.
        #[arg(long, conflicts_with = "store")]
        no_store: bool,

        /// Write no log. By default `serve` keeps one in the state directory,
        /// `logs/serve.YYYY-MM-DD.log`, seven days of it, filtered by
        /// `LUU_LOG` (`LUU_LOG=debug`). See `crate::log`.
        #[arg(long)]
        no_log: bool,

        /// A destination named in the state directory's `config.toml`, as
        /// `[provider.<name>]`: the backend, the URL, the model and the key
        /// written down once instead of retyped per run. The flags below
        /// override it field by field. See
        /// `RECORD/2026-09-07.naming-a-provider.completed.md`.
        #[arg(long, short, value_name = "NAME")]
        provider: Option<String>,

        /// The profile's backend, or `mock` when nothing named one.
        #[arg(long, value_enum)]
        backend: Option<BackendKind>,

        /// The profile's model, or `qwen2.5-coder:7b`. **One field of a
        /// destination, not a destination**: `-m` over a profile that names a
        /// local server asks *that server* for the model, and the answer is its
        /// own 404 rather than a redirect.
        #[arg(long, short)]
        model: Option<String>,

        /// The profile's URL under `--backend ollama`, or
        /// `http://127.0.0.1:11434`.
        #[arg(long)]
        ollama_url: Option<String>,

        /// Where the OpenAI-compatible server is. `llama-server`, vLLM, LM
        /// Studio and the hosted endpoints all answer here — and none of them
        /// takes the window on a request, so start the server with the window
        /// this run budgets against. Defaults to the profile's URL, then to
        /// `http://127.0.0.1:8080/v1`.
        #[arg(long)]
        openai_url: Option<String>,

        /// A file holding the bearer token for `--backend openai`. Omitted
        /// means no `Authorization` header at all, which is what a local
        /// server wants. A file rather than a flag or an env var, for the
        /// reason `crate::secret` gives — and its mode is checked there, as the
        /// auth token's always was and this one never was.
        #[arg(long, value_name = "PATH")]
        api_key_file: Option<std::path::PathBuf>,

        /// How tool calls travel: `fenced` (definitions in the system text, a
        /// ```` ```tool ```` block back) or `native` (the server's `tools`
        /// field and its own tool prompt). Over the profile's `tool-calls`.
        #[arg(long, value_enum, value_name = "HOW")]
        tool_calls: Option<provider::ToolCalls>,

        #[arg(long, default_value_t = 25)]
        mock_delay_ms: u64,

        /// What the mock backend answers, one per model call, the last
        /// repeating. The gate needs two to be seen working without a model:
        /// the plan block the planning call returns, then the answer.
        #[arg(long = "mock-reply", value_name = "TEXT")]
        mock_replies: Vec<String>,

        /// Wrap back to the first `--mock-reply` instead of repeating the
        /// last one. What turns two replies — a call, then the reply that
        /// reads its result — into a tool called every turn, for as long as
        /// the script runs, instead of once at the start of the session.
        #[arg(long)]
        mock_cycle: bool,

        /// Write every protocol and trace message to a replayable JSON-lines file.
        #[arg(long)]
        record: Option<std::path::PathBuf>,

        /// The model's context window. 0 means unknown: no budget, no eviction.
        #[arg(long, default_value_t = 0)]
        context_limit: u32,

        /// The model's `tokenizer.json`. Without it, tokens are counted
        /// approximately and every number says so.
        #[arg(long)]
        tokenizer: Option<std::path::PathBuf>,

        /// Tokens held back for the answer before history is considered.
        #[arg(long, default_value_t = DEFAULT_RESERVE)]
        reserve: u32,

        /// How the history gives way when a turn no longer fits. `turn` drops
        /// the minimum and rewrites the history on every call once the window
        /// is full; `block` cuts down to --low-water and then holds still.
        #[arg(long, value_enum, default_value_t = EvictionKind::Turn)]
        evict: EvictionKind,

        /// The fraction of the history budget `--evict block` cuts down to.
        /// Ignored by `--evict turn`. A guess, and a flag so that it can stop
        /// being one.
        #[arg(long, default_value_t = 0.5)]
        low_water: f32,

        /// Render a span once, in the oldest turn of the window that carries
        /// it, instead of in every turn that selected it. **On by default**
        /// since 2026-09-19 — the flag is kept because a `[resend]` table that
        /// says `always` is still a thing a run may want to override.
        #[arg(long)]
        repeat_once: bool,

        /// Send a span again in every turn that selected it: the rule above,
        /// off. The arm every recording made before 2026-09-19 was made under,
        /// and the only way to ask for it from a command line now that it is
        /// no longer what you get by saying nothing.
        #[arg(long, conflicts_with = "repeat_once")]
        no_repeat_once: bool,

        /// Under pressure, let the oldest turns of the window give up their
        /// spans and keep their exchange: the code becomes the line that cites
        /// it, and a turn is evicted whole only when pruning the window is not
        /// enough. Off, and it stayed off when rule A's default flipped: what
        /// it buys is measured in tokens and what it costs is not measured at
        /// all, which is item 5 of `ROADMAP/2026-09-17`.
        #[arg(long)]
        prune_behind: bool,

        /// Let a turn behind the prune line give up its tool *output* too: the
        /// call stays as the model wrote it and the result becomes the line
        /// that cites it. Off, and inert without --prune-behind — nothing is
        /// behind a line that never moves.
        #[arg(long)]
        prune_results: bool,

        /// Pin the sampler's temperature. Unset leaves it to the server's own
        /// default, which is not fixed across calls.
        #[arg(long)]
        temperature: Option<f32>,

        /// Pin the sampler's seed. Unset leaves it to the server's own choice.
        #[arg(long)]
        seed: Option<u32>,

        /// Tokens of repository outline to put in the prefix — definitions
        /// with their signatures, bodies elided, from tree-sitter. 0 is off,
        /// which is the default: a map that arrived switched on would change
        /// every number in every recording made so far. `luu map` prints what
        /// a budget resolves to.
        #[arg(long, default_value_t = 0)]
        map_tokens: u32,

        /// Order the map by what the rest of the tree depends on, instead of
        /// by path. Off, and the reason is a measurement rather than caution:
        /// at 1024 tokens the alphabet holds five files and the ranking holds
        /// two, because rank order puts the big central files first and the
        /// fill rule stops at the first that does not fit. `luu map --explain`
        /// prints the ranking either way. See
        /// `RECORD/2026-09-02.ranking-the-map.completed.md`.
        #[arg(long)]
        map_rank: bool,

        /// Order the map by direct weighted in-degree.
        #[arg(long)]
        map_in_degree: bool,

        /// Pack the token budget non-greedily.
        #[arg(long)]
        map_non_greedy: bool,

        /// Tokens of *selected fragments* to fuse into each prompt — the
        /// definitions this turn's own text points at, chosen by
        /// `agent_core::select` and read through the **live job's** sandbox, so
        /// an approved plan narrows what may be chosen exactly as it narrows
        /// what may be opened. 0 is off, which is the default and for the same
        /// reason the map's is. Unlike the map it is not part of the cached
        /// prefix. See `RECORD/2026-09-06.selection-at-the-gate.completed.md`.
        #[arg(long, default_value_t = 0)]
        select_tokens: u32,

        /// Score a file by its own module doc as well as by what it defines.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        select_docs: bool,

        /// Also score a file for being one hop from a file that matched. Off:
        /// measured worse than without it. See
        /// `RECORD/2026-09-05.choosing-fragments.completed.md`.
        #[arg(long)]
        select_graph: bool,

        /// `schema` (`response_format`) or `grammar` (GBNF), for every turn of
        /// the session. Off, so a run made without it stays comparable to
        /// every recording on disk — same flag `chat` carries, see its own doc
        /// for what the two arms cost. See
        /// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
        #[arg(long, value_enum)]
        constrain: Option<ConstrainKind>,
    },

    /// Serve the agent protocol over stdin/stdout as NDJSON.
    ///
    /// The editor surface: an IDE (VSCode, Neovim, Emacs) or language server
    /// spawns `luu stdio` as a subprocess and speaks the protocol directly over
    /// standard input and output. The gate, tasks, tools, and store behave
    /// identically to `luu serve` without needing a network port or authentication token.
    Stdio {
        #[command(flatten)]
        sandbox_args: SandboxArgs,

        /// Where sessions are cached between restarts, as SQLite. Defaults to
        /// `sessions.db` in the state directory the first run chose
        /// (`~/.luu` or `~/.config/luu`, or wherever `LUU_HOME` names);
        /// `--no-store` keeps the session in memory,
        /// which is what `serve` did before the store existed.
        #[arg(long, value_name = "PATH")]
        store: Option<std::path::PathBuf>,

        /// Keep the session in memory only.
        #[arg(long, conflicts_with = "store")]
        no_store: bool,

        /// A destination named in the state directory's `config.toml`, as
        /// `[provider.<name>]`: the backend, the URL, the model and the key
        /// written down once instead of retyped per run. The flags below
        /// override it field by field. See
        /// `RECORD/2026-09-07.naming-a-provider.completed.md`.
        #[arg(long, short, value_name = "NAME")]
        provider: Option<String>,

        /// The profile's backend, or `mock` when nothing named one.
        #[arg(long, value_enum)]
        backend: Option<BackendKind>,

        /// The profile's model, or `qwen2.5-coder:7b`. **One field of a
        /// destination, not a destination**: `-m` over a profile that names a
        /// local server asks *that server* for the model, and the answer is its
        /// own 404 rather than a redirect.
        #[arg(long, short)]
        model: Option<String>,

        /// The profile's URL under `--backend ollama`, or
        /// `http://127.0.0.1:11434`.
        #[arg(long)]
        ollama_url: Option<String>,

        /// Where the OpenAI-compatible server is. `llama-server`, vLLM, LM
        /// Studio and the hosted endpoints all answer here — and none of them
        /// takes the window on a request, so start the server with the window
        /// this run budgets against. Defaults to the profile's URL, then to
        /// `http://127.0.0.1:8080/v1`.
        #[arg(long)]
        openai_url: Option<String>,

        /// A file holding the bearer token for `--backend openai`. Omitted
        /// means no `Authorization` header at all, which is what a local
        /// server wants.
        #[arg(long, value_name = "PATH")]
        api_key_file: Option<std::path::PathBuf>,

        /// How tool calls travel: `fenced` (definitions in the system text, a
        /// ```` ```tool ```` block back) or `native` (the server's `tools`
        /// field and its own tool prompt). Over the profile's `tool-calls`.
        #[arg(long, value_enum, value_name = "HOW")]
        tool_calls: Option<provider::ToolCalls>,

        #[arg(long, default_value_t = 25)]
        mock_delay_ms: u64,

        /// What the mock backend answers, one per model call, the last
        /// repeating. The gate needs two to be seen working without a model:
        /// the plan block the planning call returns, then the answer.
        #[arg(long = "mock-reply", value_name = "TEXT")]
        mock_replies: Vec<String>,

        /// Wrap back to the first `--mock-reply` instead of repeating the
        /// last one. What turns two replies — a call, then the reply that
        /// reads its result — into a tool called every turn, for as long as
        /// the script runs, instead of once at the start of the session.
        #[arg(long)]
        mock_cycle: bool,

        /// Write every protocol and trace message to a replayable JSON-lines file.
        #[arg(long)]
        record: Option<std::path::PathBuf>,

        /// The model's context window. 0 means unknown: no budget, no eviction.
        #[arg(long, default_value_t = 0)]
        context_limit: u32,

        /// The model's `tokenizer.json`. Without it, tokens are counted
        /// approximately and every number says so.
        #[arg(long)]
        tokenizer: Option<std::path::PathBuf>,

        /// Tokens held back for the answer before history is considered.
        #[arg(long, default_value_t = DEFAULT_RESERVE)]
        reserve: u32,

        /// How the history gives way when a turn no longer fits. `turn` drops
        /// the minimum and rewrites the history on every call once the window
        /// is full; `block` cuts down to --low-water and then holds still.
        #[arg(long, value_enum, default_value_t = EvictionKind::Turn)]
        evict: EvictionKind,

        /// The fraction of the history budget `--evict block` cuts down to.
        /// Ignored by `--evict turn`. A guess, and a flag so that it can stop
        /// being one.
        #[arg(long, default_value_t = 0.5)]
        low_water: f32,

        /// Render a span once, in the oldest turn of the window that carries
        /// it, instead of in every turn that selected it. **On by default**
        /// since 2026-09-19 — the flag is kept because a `[resend]` table that
        /// says `always` is still a thing a run may want to override.
        #[arg(long)]
        repeat_once: bool,

        /// Send a span again in every turn that selected it: the rule above,
        /// off. The arm every recording made before 2026-09-19 was made under,
        /// and the only way to ask for it from a command line now that it is
        /// no longer what you get by saying nothing.
        #[arg(long, conflicts_with = "repeat_once")]
        no_repeat_once: bool,

        /// Under pressure, let the oldest turns of the window give up their
        /// spans and keep their exchange: the code becomes the line that cites
        /// it, and a turn is evicted whole only when pruning the window is not
        /// enough. Off, and it stayed off when rule A's default flipped: what
        /// it buys is measured in tokens and what it costs is not measured at
        /// all, which is item 5 of `ROADMAP/2026-09-17`.
        #[arg(long)]
        prune_behind: bool,

        /// Let a turn behind the prune line give up its tool *output* too: the
        /// call stays as the model wrote it and the result becomes the line
        /// that cites it. Off, and inert without --prune-behind — nothing is
        /// behind a line that never moves.
        #[arg(long)]
        prune_results: bool,

        /// Pin the sampler's temperature. Unset leaves it to the server's own
        /// default, which is not fixed across calls.
        #[arg(long)]
        temperature: Option<f32>,

        /// Pin the sampler's seed. Unset leaves it to the server's own choice.
        #[arg(long)]
        seed: Option<u32>,

        /// Tokens of repository outline to put in the prefix — definitions
        /// with their signatures, bodies elided, from tree-sitter. 0 is off,
        /// which is the default: a map that arrived switched on would change
        /// every number in every recording made so far. `luu map` prints what
        /// a budget resolves to.
        #[arg(long, default_value_t = 0)]
        map_tokens: u32,

        /// Order the map by what the rest of the tree depends on, instead of
        /// by path. Off, and the reason is a measurement rather than caution:
        /// at 1024 tokens the alphabet holds five files and the ranking holds
        /// two, because rank order puts the big central files first and the
        /// fill rule stops at the first that does not fit. `luu map --explain`
        /// prints the ranking either way. See
        /// `RECORD/2026-09-02.ranking-the-map.completed.md`.
        #[arg(long)]
        map_rank: bool,

        /// Order the map by direct weighted in-degree.
        #[arg(long)]
        map_in_degree: bool,

        /// Pack the token budget non-greedily.
        #[arg(long)]
        map_non_greedy: bool,

        /// Tokens of *selected fragments* to fuse into each prompt — the
        /// definitions this turn's own text points at, chosen by
        /// `agent_core::select` and read through the **live job's** sandbox, so
        /// an approved plan narrows what may be chosen exactly as it narrows
        /// what may be opened. 0 is off, which is the default and for the same
        /// reason the map's is. Unlike the map it is not part of the cached
        /// prefix. See `RECORD/2026-09-06.selection-at-the-gate.completed.md`.
        #[arg(long, default_value_t = 0)]
        select_tokens: u32,

        /// Score a file by its own module doc as well as by what it defines.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        select_docs: bool,

        /// Also score a file for being one hop from a file that matched. Off:
        /// measured worse than without it. See
        /// `RECORD/2026-09-05.choosing-fragments.completed.md`.
        #[arg(long)]
        select_graph: bool,

        /// `schema` (`response_format`) or `grammar` (GBNF), for every turn of
        /// the session. Off, so a run made without it stays comparable to
        /// every recording on disk — same flag `chat` carries, see its own doc
        /// for what the two arms cost. See
        /// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
        #[arg(long, value_enum)]
        constrain: Option<ConstrainKind>,
    },

    /// Run a turn — or a scripted sequence of them — streaming to stdout.
    Chat {
        /// The prompt. Reads stdin when omitted, and ignored with --script.
        prompt: Option<String>,

        #[command(flatten)]
        sandbox_args: SandboxArgs,

        /// A file of prompts, one per line, run in order against one shared
        /// history. `#` comments and blank lines are skipped. This is how a
        /// multi-turn baseline gets recorded the same way twice.
        #[arg(long)]
        script: Option<std::path::PathBuf>,

        /// Fuse a file into the next prompt, as `PATH` or `PATH:START-END`.
        /// Repeatable, read through the sandbox, and attached to **one** turn —
        /// which turns a file belongs in is what relevance selection exists to
        /// decide later. In a script, `## fragment: <path>` does the same thing
        /// at the point it appears.
        #[arg(long = "fragment", value_name = "PATH[:START-END]")]
        fragments: Vec<String>,

        /// A destination named in the state directory's `config.toml`, as
        /// `[provider.<name>]`: the backend, the URL, the model and the key
        /// written down once instead of retyped per run. The flags below
        /// override it field by field. See
        /// `RECORD/2026-09-07.naming-a-provider.completed.md`.
        #[arg(long, short, value_name = "NAME")]
        provider: Option<String>,

        /// The profile's backend, or `mock` when nothing named one.
        #[arg(long, value_enum)]
        backend: Option<BackendKind>,

        /// The profile's model, or `qwen2.5-coder:7b`. **One field of a
        /// destination, not a destination**: `-m` over a profile that names a
        /// local server asks *that server* for the model, and the answer is its
        /// own 404 rather than a redirect.
        #[arg(long, short)]
        model: Option<String>,

        /// The profile's URL under `--backend ollama`, or
        /// `http://127.0.0.1:11434`.
        #[arg(long)]
        ollama_url: Option<String>,

        /// Where the OpenAI-compatible server is. `llama-server`, vLLM, LM
        /// Studio and the hosted endpoints all answer here — and none of them
        /// takes the window on a request, so start the server with the window
        /// this run budgets against. Defaults to the profile's URL, then to
        /// `http://127.0.0.1:8080/v1`.
        #[arg(long)]
        openai_url: Option<String>,

        /// A file holding the bearer token for `--backend openai`. Omitted
        /// means no `Authorization` header at all, which is what a local
        /// server wants. A file rather than a flag or an env var, for the
        /// reason `crate::secret` gives — and its mode is checked there, as the
        /// auth token's always was and this one never was.
        #[arg(long, value_name = "PATH")]
        api_key_file: Option<std::path::PathBuf>,

        /// How tool calls travel: `fenced` (definitions in the system text, a
        /// ```` ```tool ```` block back) or `native` (the server's `tools`
        /// field and its own tool prompt). Over the profile's `tool-calls`.
        #[arg(long, value_enum, value_name = "HOW")]
        tool_calls: Option<provider::ToolCalls>,

        /// Milliseconds between mock tokens, for exercising slow generation.
        #[arg(long, default_value_t = 25)]
        mock_delay_ms: u64,

        /// What the mock backend answers, one per model call. Repeatable, and
        /// the last one repeats — which is how the tool loop is exercised end
        /// to end without a model: a reply containing a ```tool block, then the
        /// reply that reads its result.
        #[arg(long = "mock-reply", value_name = "TEXT")]
        mock_replies: Vec<String>,

        /// Wrap back to the first `--mock-reply` instead of repeating the
        /// last one, so a script of any length gets a tool call on every
        /// turn instead of one at the start of the session.
        #[arg(long)]
        mock_cycle: bool,

        /// Stop the turn after this many milliseconds, to exercise cancelling.
        #[arg(long)]
        cancel_after_ms: Option<u64>,

        /// Write the turn to a replayable JSON-lines file. The same format
        /// `luu serve --record` writes, so the UI can load either.
        #[arg(long)]
        record: Option<std::path::PathBuf>,

        /// The model's context window. 0 means unknown: no budget, no eviction.
        #[arg(long, default_value_t = 0)]
        context_limit: u32,

        /// The model's `tokenizer.json`. Without it, tokens are counted
        /// approximately and every number says so.
        #[arg(long)]
        tokenizer: Option<std::path::PathBuf>,

        /// Tokens held back for the answer before history is considered.
        #[arg(long, default_value_t = DEFAULT_RESERVE)]
        reserve: u32,

        /// How the history gives way when a turn no longer fits. `turn` drops
        /// the minimum and rewrites the history on every call once the window
        /// is full; `block` cuts down to --low-water and then holds still.
        #[arg(long, value_enum, default_value_t = EvictionKind::Turn)]
        evict: EvictionKind,

        /// The fraction of the history budget `--evict block` cuts down to.
        /// Ignored by `--evict turn`. A guess, and a flag so that it can stop
        /// being one.
        #[arg(long, default_value_t = 0.5)]
        low_water: f32,

        /// Render a span once, in the oldest turn of the window that carries
        /// it, instead of in every turn that selected it. **On by default**
        /// since 2026-09-19 — the flag is kept because a `[resend]` table that
        /// says `always` is still a thing a run may want to override.
        #[arg(long)]
        repeat_once: bool,

        /// Send a span again in every turn that selected it: the rule above,
        /// off. The arm every recording made before 2026-09-19 was made under,
        /// and the only way to ask for it from a command line now that it is
        /// no longer what you get by saying nothing.
        #[arg(long, conflicts_with = "repeat_once")]
        no_repeat_once: bool,

        /// Under pressure, let the oldest turns of the window give up their
        /// spans and keep their exchange: the code becomes the line that cites
        /// it, and a turn is evicted whole only when pruning the window is not
        /// enough. Off, and it stayed off when rule A's default flipped: what
        /// it buys is measured in tokens and what it costs is not measured at
        /// all, which is item 5 of `ROADMAP/2026-09-17`.
        #[arg(long)]
        prune_behind: bool,

        /// Let a turn behind the prune line give up its tool *output* too: the
        /// call stays as the model wrote it and the result becomes the line
        /// that cites it. Off, and inert without --prune-behind — nothing is
        /// behind a line that never moves.
        #[arg(long)]
        prune_results: bool,

        /// Pin the sampler's temperature, so two runs meant to be compared
        /// differ only by what they're testing. Unset leaves it to the
        /// server's own default.
        #[arg(long)]
        temperature: Option<f32>,

        /// Pin the sampler's seed. Unset leaves it to the server's own choice.
        #[arg(long)]
        seed: Option<u32>,

        /// Tokens of repository outline to put in the prefix — definitions
        /// with their signatures, bodies elided, from tree-sitter. 0 is off,
        /// which is the default: a map that arrived switched on would change
        /// every number in every recording made so far. `luu map` prints what
        /// a budget resolves to.
        #[arg(long, default_value_t = 0)]
        map_tokens: u32,

        /// Order the map by what the rest of the tree depends on, instead of
        /// by path. Off, and the reason is a measurement rather than caution:
        /// at 1024 tokens the alphabet holds five files and the ranking holds
        /// two, because rank order puts the big central files first and the
        /// fill rule stops at the first that does not fit. `luu map --explain`
        /// prints the ranking either way. See
        /// `RECORD/2026-09-02.ranking-the-map.completed.md`.
        #[arg(long)]
        map_rank: bool,

        /// Order the map by direct weighted in-degree.
        #[arg(long)]
        map_in_degree: bool,

        /// Pack the token budget non-greedily.
        #[arg(long)]
        map_non_greedy: bool,

        /// Tokens of *selected fragments* to fuse into each prompt — the
        /// definitions this turn's own text points at, chosen by
        /// `agent_core::select`. 0 is off, which is the default and for the
        /// same reason the map's is: it changes every number in every recording
        /// made before it. Unlike the map it is **not** part of the cached
        /// prefix, so a run with it on is not comparable on prefix reuse to one
        /// without. `luu select` prints what a budget resolves to.
        #[arg(long, default_value_t = 0)]
        select_tokens: u32,

        /// Score a file by its own module doc as well as by what it defines.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        select_docs: bool,

        /// Also score a file for being one hop from a file that matched. Off:
        /// measured worse than without it. See
        /// `RECORD/2026-09-05.choosing-fragments.completed.md`.
        #[arg(long)]
        select_graph: bool,

        /// `schema` (`response_format`) or `grammar` (GBNF). Off, so a run
        /// made without it stays comparable to every recording on disk. The
        /// two arms constrain differently, not just differently-spelled:
        /// `grammar` constrains every reply from the first attempt — prose
        /// stays reachable through the alternation. `schema` has no "just
        /// answer" branch, so it is never sent on a first attempt; it is
        /// spent once, as `SchemaRetry`, only after an unconstrained first
        /// reply drifted (`CallVerdict::Drifted` — fenced under a tool's own
        /// name, not a decision not to call). A backend that cannot render
        /// the one asked for says so once and sends its own request
        /// unconstrained regardless — see `Backend::constrain_caveat`.
        #[arg(long, value_enum)]
        constrain: Option<ConstrainKind>,
    },
}

/// The flag, as the map's own type.
fn order_of(rank: bool, in_degree: bool) -> Order {
    if in_degree {
        Order::InDegree
    } else if rank {
        Order::Ranked
    } else {
        Order::Path
    }
}

/// The selector's weights, from the two flags that switch a signal on or off.
///
/// Two flags and not five: the individual weights are numbers measured against
/// the corpus and are not the command line's business, while *which signals are
/// in play at all* is exactly what a person comparing two runs needs to say.
fn weights_of(docs: bool, graph: bool) -> agent_core::select::Weights {
    let weights = agent_core::select::Weights::default();
    let weights = match docs {
        true => weights,
        false => weights.without_docs(),
    };
    match graph {
        true => weights.with_graph(),
        false => weights,
    }
}

fn fill_of(non_greedy: bool) -> agent_core::repo_map::Fill {
    if non_greedy {
        agent_core::repo_map::Fill::NonGreedy
    } else {
        agent_core::repo_map::Fill::Greedy
    }
}

#[derive(Subcommand)]
enum KeyAction {
    /// Make a key, write the private half, and print the block to paste into
    /// `luu.toml`.
    New {
        /// Where the private half goes. Written `0600`, for the reason
        /// `crate::auth` reads its token out of a file: a flag is greppable in
        /// `ps` and an environment variable is inherited by every child.
        #[arg(long, short, value_name = "FILE")]
        out: std::path::PathBuf,

        /// What the host's `luu.toml` will call this key. Names are per host.
        #[arg(long, default_value = "operator")]
        name: String,
    },

    /// Sign an `approve_job` message read on stdin, and write it back out with
    /// its signature attached.
    Sign {
        /// The private half, as `luu key new --out` wrote it.
        #[arg(long, short, value_name = "FILE")]
        key: std::path::PathBuf,

        /// The session the approval belongs to, as the host's `hello` names
        /// it. In the signed bytes, so a signature captured here does not
        /// replay against another host that numbers its jobs the same way.
        #[arg(long, value_name = "ID")]
        session: String,

        /// The name the host's `luu.toml` calls this key.
        #[arg(long = "as", default_value = "operator")]
        signer: String,
    },
}

/// `luu key`: the half of signed approvals that produces a signature.
fn run_key(action: &KeyAction) -> Result<()> {
    match action {
        KeyAction::New { out, name } => {
            let signer = Signer::generate()?;
            write_private(out, &signer.secret())?;
            eprintln!("wrote {} (mode 0600)", out.display());
            println!("[[approvals.key]]");
            println!("name = \"{name}\"");
            println!("public = \"{}\"", signer.public());
            Ok(())
        }
        KeyAction::Sign {
            key,
            session,
            signer,
        } => {
            let signing = Signer::from_file(key)?;
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
                .context("reading the approval on stdin")?;
            let message: ServerBoundMessage =
                serde_json::from_str(input.trim()).context("parsing the approval on stdin")?;
            let ServerBoundMessage::ApprovePlan {
                job,
                files,
                writes,
                commands,
                closes_on,
                network,
                enforcement,
                egress,
                ..
            } = message
            else {
                anyhow::bail!("only an approve_plan message can be signed");
            };
            // The id the approval will hand out, which the message names for
            // exactly this: a signature bound to no job could be replayed
            // against any of them.
            let Some(job) = job else {
                anyhow::bail!(
                    "an approval to be signed must name the job it will open; \
                     add `\"job\": <the session's next job id>`"
                );
            };
            let signature = signing.sign(
                &Approval {
                    session,
                    job,
                    files: &files,
                    writes: &writes,
                    commands: &commands,
                    closes_on: closes_on.as_ref(),
                    network,
                    egress: egress.as_ref(),
                    enforcement,
                },
                signer.clone(),
            )?;
            let signed = ServerBoundMessage::ApprovePlan {
                job: Some(job),
                files,
                writes,
                commands,
                closes_on,
                network,
                enforcement,
                egress,
                signature: Some(signature),
            };
            println!("{}", serde_json::to_string(&signed)?);
            Ok(())
        }
    }
}

/// The private half, written where only its owner can read it — the same rule
/// `crate::auth` holds the bearer token to, and for the same reason.
fn write_private(path: &std::path::Path, text: &str) -> Result<()> {
    std::fs::write(path, format!("{text}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting the mode of {}", path.display()))?;
    }
    Ok(())
}

/// The sandbox flags, shared by every subcommand that can act.
///
/// They *add* to whatever the policy file said rather than replacing it, so
/// widening a project's sandbox for one run is a flag and narrowing it is an
/// edit to the file — which is the direction that should be the harder one.
#[derive(Clone, clap::Args)]
pub(crate) struct SandboxArgs {
    /// Sandbox policy (TOML). Defaults to ./luu.toml when it is there.
    #[arg(long, value_name = "FILE")]
    sandbox: Option<std::path::PathBuf>,

    /// Grant read access to a path. Repeatable.
    #[arg(long = "allow-read", value_name = "PATH")]
    allow_read: Vec<std::path::PathBuf>,

    /// Grant read and write. Repeatable.
    #[arg(long = "allow-write", value_name = "PATH")]
    allow_write: Vec<std::path::PathBuf>,

    /// Grant read and the right to run what is in the tree. Repeatable.
    #[arg(long = "allow-exec", value_name = "PATH")]
    allow_exec: Vec<std::path::PathBuf>,

    /// Let run_command run this program. Repeatable, and a program name — never
    /// a shell string, which would make the list it is checked against
    /// meaningless.
    #[arg(long = "allow-command", value_name = "NAME")]
    allow_command: Vec<String>,

    /// Let subprocesses reach the network.
    #[arg(long = "allow-network")]
    allow_network: bool,

    /// What to do where the kernel cannot hold a subprocess: `kernel` denies
    /// the call and says what is missing, `best-effort` runs it and reports the
    /// gap in every verdict.
    #[arg(long = "sandbox-enforcement", value_enum)]
    enforcement: Option<EnforcementKind>,

    /// Tool calls one turn may make before it has to answer.
    #[arg(long, default_value_t = DEFAULT_MAX_STEPS)]
    max_tool_steps: u32,

    /// Run without tools. The prefix loses the definitions block, so this is
    /// not the same prompt with the tools ignored — it is a different one.
    #[arg(long)]
    no_tools: bool,

    /// Where tool calls run: `host` (this process), `direct` (a `luu worker`
    /// child, no container), or a container runtime — `docker`, `podman`,
    /// `nerdctl`, `container`, `colima`. Overrides `[worker] runtime`.
    ///
    /// `direct` isolates nothing and says so in every line that reports it. It
    /// exists so the seam can be exercised where no runtime is installed.
    #[arg(long = "worker", value_name = "RUNTIME")]
    worker: Option<Runtime>,

    /// The image the worker runs in. Overrides `[worker] image`.
    #[arg(long = "worker-image", value_name = "REF")]
    worker_image: Option<String>,

    /// Where `luu` is, for `--worker direct`. Defaults to this binary.
    #[arg(long = "worker-binary", value_name = "PATH")]
    worker_binary: Option<std::path::PathBuf>,

    /// How long the seam waits for a tool call that has no clock of its own
    /// before it kills the worker and starts another. A `run_command` gets this
    /// *plus* the `timeout_ms` it asked for, so the seam never fires first.
    /// Overrides `[worker] timeout-ms`.
    #[arg(long = "worker-timeout-ms", value_name = "MS")]
    worker_timeout_ms: Option<u64>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum EnforcementKind {
    Kernel,
    BestEffort,
}

impl SandboxArgs {
    /// Resolves the policy: the file, then the flags, then the working
    /// directory as the base every relative path is taken against.
    ///
    /// Async because the last step may be starting a worker, and a worker is
    /// not started until its handshake has come back: a session that announced
    /// a container and then could not reach one would be a session whose
    /// verdicts lie.
    /// The file `[sandbox]`, `[worker]` and `[approvals]` are all read from —
    /// three readers of one file, which is why the path is resolved in one
    /// place rather than three.
    fn policy_path(&self) -> Result<std::path::PathBuf> {
        let base = std::env::current_dir().context("the working directory")?;
        Ok(self
            .sandbox
            .clone()
            .unwrap_or_else(|| base.join("luu.toml")))
    }

    /// `[approvals]`: who may approve, and whether anyone must sign to. A file
    /// without the block is not an error — it is a `luu.toml` from before
    /// signatures existed, and its approvals are the operator's own.
    fn approvers(&self) -> Result<Approvers> {
        let path = self.policy_path()?;
        match path.exists() {
            true => {
                Approvers::from_file(&path).with_context(|| format!("reading {}", path.display()))
            }
            false => Ok(Approvers::default()),
        }
    }

    /// The `[worker]` block of a policy file, and the flags over it. A
    /// `luu.toml` from before this existed has no block and resolves to
    /// `host`, which is the run every measurement in this repository was made
    /// under.
    fn worker_config(&self, path: &std::path::Path) -> Result<WorkerConfig> {
        let mut worker_config = match path.exists() {
            true => WorkerConfig::from_file(path)
                .with_context(|| format!("reading {}", path.display()))?,
            false => WorkerConfig::default(),
        };
        if let Some(runtime) = self.worker {
            worker_config.runtime = runtime;
        }
        if self.worker_image.is_some() {
            worker_config.image = self.worker_image.clone();
        }
        if let Some(timeout_ms) = self.worker_timeout_ms {
            worker_config.timeout_ms = timeout_ms;
        }
        Ok(worker_config)
    }

    /// Where a posture would run, **without building it**: the `[worker]`
    /// block and the flags over it, and whether this machine has the runtime.
    /// Building is what starts a container, and a picker that started one per
    /// option to say what each one is would be a picker that cost a VM boot to
    /// open. An image that is not built is not caught here — that needs the
    /// runtime to answer, and the move itself says so when it is refused. See
    /// `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
    ///
    /// `runtime` is a variant: the same file on another container runtime,
    /// the way `--worker` moves every posture at once. The caller only asks it
    /// of a posture that is already contained.
    pub(crate) fn describe_posture(
        &self,
        policy: Option<&std::path::Path>,
        runtime: Option<agent_core::worker::Runtime>,
    ) -> serve::Where {
        let path = match policy {
            Some(path) => Ok(path.to_path_buf()),
            None => self.policy_path(),
        };
        let shown = |path: &std::path::Path| match path.exists() {
            true => path.display().to_string(),
            false => format!("{} (absent: the built-in policy)", path.display()),
        };
        let (path, config) = match path {
            Ok(path) => {
                let config = self.worker_config(&path).map(|mut config| {
                    if let Some(runtime) = runtime {
                        config.runtime = runtime;
                    }
                    config
                });
                (path, config)
            }
            Err(error) => {
                return serve::Where {
                    policy: String::new(),
                    runtime: "host".into(),
                    image: None,
                    missing: Some(format!("{error:#}")),
                    gap: None,
                };
            }
        };
        match config {
            Err(error) => serve::Where {
                policy: shown(&path),
                runtime: "host".into(),
                image: None,
                missing: Some(format!("{error:#}")),
                gap: None,
            },
            Ok(config) => {
                let contained = config.runtime.is_contained();
                let missing = match config.runtime.program() {
                    Some(program) if agent_core::worker::which(program).is_none() => {
                        Some(format!("`{program}` is not installed on this machine"))
                    }
                    Some(_) if config.image.is_none() => {
                        Some("the policy file names no [worker] image".to_string())
                    }
                    _ => None,
                };
                serve::Where {
                    policy: shown(&path),
                    runtime: config.runtime.as_str().to_string(),
                    image: config.image.filter(|_| contained),
                    missing,
                    gap: config.runtime.cannot().map(str::to_string),
                }
            }
        }
    }

    /// The same resolution, against a policy file a *session* named rather than
    /// the one this process was started with.
    ///
    /// A clone with one field replaced, so every other flag — the extra grants,
    /// the enforcement override, the worker runtime and image — means what it
    /// meant, and the rule that an explicit policy file must exist is the same
    /// rule. See `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
    ///
    /// `runtime` is a variant, as in [`SandboxArgs::describe_posture`]: the
    /// `--worker` flag, for one session.
    pub(crate) async fn resolve_posture(
        &self,
        policy: Option<&std::path::Path>,
        runtime: Option<agent_core::worker::Runtime>,
    ) -> Result<Agency> {
        if policy.is_none() && runtime.is_none() {
            return self.resolve().await;
        }
        let mut args = self.clone();
        if let Some(path) = policy {
            args.sandbox = Some(path.to_path_buf());
        }
        if runtime.is_some() {
            args.worker = runtime;
        }
        args.resolve().await
    }

    async fn resolve(&self) -> Result<Agency> {
        let base = std::env::current_dir().context("the working directory")?;

        let explicit = self.sandbox.is_some();
        let path = self.policy_path()?;
        let mut policy = match (explicit, path.exists()) {
            // An explicit --sandbox that is not there is an error: it was asked
            // for, and falling back to the default would be a wider sandbox
            // than the one the user named.
            (true, false) => anyhow::bail!("--sandbox {}: no such file", path.display()),
            (_, true) => SandboxPolicy::from_file(&path)
                .with_context(|| format!("reading {}", path.display()))?,
            (false, false) => SandboxPolicy::default(),
        };

        for granted in &self.allow_read {
            policy.allow(granted, Access::Read);
        }
        for granted in &self.allow_exec {
            policy.allow(granted, Access::Execute);
        }
        for granted in &self.allow_write {
            policy.allow(granted, Access::ReadWrite);
        }
        for command in &self.allow_command {
            policy.allow_command(command);
        }
        policy.network |= self.allow_network;
        if let Some(enforcement) = self.enforcement {
            policy.enforcement = match enforcement {
                EnforcementKind::Kernel => Enforcement::Kernel,
                EnforcementKind::BestEffort => Enforcement::BestEffort,
            };
        }

        let worker_config = self.worker_config(&path)?;

        let sandbox = Sandbox::new(&policy, &base)?;
        let worker = match worker_config.runtime.is_worker() {
            false => None,
            true => {
                let spec = WorkerSpec::new(worker_config.runtime, sandbox.base())
                    .with_image(worker_config.image.clone())
                    // How the container is *created*, and never changed
                    // afterwards. A task that may not reach the network inside
                    // a session that may is the per-spawn seccomp filter's job.
                    .with_network(sandbox.network())
                    .with_binary(self.worker_binary.clone())
                    // The image's own trees, which the host must not try to
                    // resolve: `/usr/local/cargo` is the image's toolchain and
                    // is not a directory here.
                    .with_paths(worker_config.paths.clone())
                    // The seam's clock: how long a call with no clock of its
                    // own may take before the worker is treated as stuck. See
                    // `RECORD/2026-09-05.a-clock-at-the-seam.completed.md`.
                    .with_timeout_ms(worker_config.timeout_ms);
                // What the runtime cannot express, said once, where a person
                // reads it — rather than silently dropped from the argv. Before
                // the start rather than after it, so a run that fails for an
                // unrelated reason still says what it would have been missing.
                if let Some(gap) = worker_config.runtime.cannot() {
                    eprintln!("warning: {}: {gap}", worker_config.runtime);
                }
                let worker = Worker::start(&spec, sandbox.commands())
                    .await
                    .with_context(|| format!("the {} worker", spec.label()))?;
                Some(std::sync::Arc::new(worker))
            }
        };

        Ok(Agency::new(
            std::sync::Arc::new(match self.no_tools {
                true => Tools::new(Vec::new()),
                false => Tools::standard(),
            }),
            std::sync::Arc::new(sandbox),
            agent_core::agent::Limits::default()
                .with_max_steps(self.max_tool_steps)
                .with_tool_timeout(std::time::Duration::from_millis(worker_config.timeout_ms)),
            worker,
        ))
    }
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum EvictionKind {
    Turn,
    Block,
}

impl EvictionKind {
    fn policy(self, low_water: f32) -> Eviction {
        match self {
            Self::Turn => Eviction::Turn,
            Self::Block => Eviction::Block { low_water },
        }
    }
}

/// `--constrain` as the two bets `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`
/// named. Neither is the default: unconstrained is what every recording on
/// disk before this flag existed was measured under, and a flag that
/// changed the default would make them incomparable retroactively.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum ConstrainKind {
    /// `response_format`: every reply becomes a call to exactly one tool.
    /// No "just answer" branch — see `Tools::call_schema`'s own doc for why
    /// that makes this a retry's constraint, not a first attempt's.
    Schema,
    /// GBNF, `agent_core::grammar::compile`: prose stays reachable, and the
    /// one fenced shape the preamble asks for is the only fenced shape that
    /// is — see `RECORD/2026-09-14.the-alternation-that-was-not-one.completed.md`
    /// for exactly what that does and does not close off.
    Grammar,
}

impl ConstrainKind {
    // `pub`: `ServeOptions`/`StdioOptions` carry the raw flag and build it in
    // `App::create`, once, the same reason `chat` builds it once — see
    // `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
    pub fn build(self, tools: &Tools) -> anyhow::Result<Constraint> {
        Ok(match self {
            Self::Schema => Constraint::Schema(tools.call_schema()),
            Self::Grammar => Constraint::Grammar(
                agent_core::grammar::compile(tools)
                    .map_err(|error| anyhow::anyhow!("compiling the grammar: {error}"))?,
            ),
        })
    }
}

/// The three rules a run starts on: this machine's `[resend]` table, with the
/// flags a run was given on top of it.
///
/// A `bool` at each flag because that is what a flag is; a named rule
/// everywhere below, because `true` at a call site says nothing about which of
/// the two rules is on. `--prune-results` stays its own flag rather than a
/// widening of `--prune-behind`: there is a run on disk made under that one,
/// and a flag that quietly starts meaning something else turns a recorded arm
/// into an unrecorded one.
///
/// That last sentence is why `--prune-results` still means `Results::Cited` now
/// that there are three values and not two. `Results::CitedReads` — cite what
/// could be read again, keep what a command found — is reachable from
/// `[resend]` and from the page, and **not from this flag**, because the run on
/// disk was made under the flag as it meant `Cited`. A seam between the two
/// surfaces, named here rather than closed by widening a recorded arm.
///
/// **The precedence is one-way for two of the three rules, and both ways for
/// the one whose default moved.** A `bool` flag can only turn a rule *on*, so
/// for `prune` and `results` *absent* and *off* arrive here as the same `false`
/// and nothing can tell them apart: a flag beats a file that says otherwise, and
/// a file that turns one of those two on cannot be turned off from a command
/// line. That is a limitation and it is pinned as one in the test.
///
/// `repeat` is the exception because its default flipped, which is exactly the
/// condition §What it costs said would owe an off-switch: `--no-repeat-once` is
/// that switch, so this argument is an `Option<bool>` where the other two are
/// `bool` — `Some` is *a flag was typed and this is which*, `None` is *nothing
/// was typed*, and the file is asked only in the `None` case. Without it, the
/// flip would make `always` unreachable from any command line for the first
/// time, which is the arm every recording before 2026-09-19 was made under.
///
/// Read by `chat` and `stdio` as well as by `serve`. It is a fact about this
/// machine, in the file that already holds this machine's default destination —
/// not a session-start choice, which is the seam the record leaves open between
/// the CLI and the page. See
/// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md` part 3.
/// The two halves of `--repeat-once` as the one answer they are: `None` where
/// neither flag was typed, and that is what lets the file be asked.
///
/// Two `bool`s arrive rather than one `Option<bool>` because that is the shape
/// clap gives a pair of flags. `conflicts_with` makes typing both a parse
/// error, so the disagreeing arm is unreachable — it is written as the
/// off-switch winning rather than left to fall out of the order the arms happen
/// to be in.
fn repeat_asked(once: bool, off: bool) -> Option<bool> {
    match (once, off) {
        (false, false) => None,
        (_, true) => Some(false),
        (true, false) => Some(true),
    }
}

fn resend_rules(
    table: crate::provider::Resend,
    once: Option<bool>,
    behind: bool,
    cited: bool,
) -> (Repeat, Prune, Results) {
    (
        match once {
            Some(true) => Repeat::Once,
            Some(false) => Repeat::Always,
            None => table.repeat.unwrap_or(Repeat::Once),
        },
        match behind {
            true => Prune::Behind,
            false => table.prune.unwrap_or(Prune::Never),
        },
        match cited {
            true => Results::Cited,
            false => table.results.unwrap_or(Results::Kept),
        },
    )
}

/// The `[resend]` table this machine names, for a surface that has not already
/// loaded the file.
///
/// A config that will not load is not a reason to refuse to run — it is already
/// reported where the providers are — so an unreadable file is the same as one
/// that says nothing, which is the defaults.
fn machine_resend() -> crate::provider::Resend {
    crate::provider::Config::load()
        .map(|(config, _)| config.resend().unwrap_or_default())
        .unwrap_or_default()
}

/// The three rules as a `Budget` takes them, for a call site that has the flags
/// and wants one expression.
fn budget_under(budget: Budget, (repeat, prune, results): (Repeat, Prune, Results)) -> Budget {
    budget.repeating(repeat).pruning(prune).citing(results)
}

/// Everything a run needs to reach a model: what the flags said, and what the
/// mock needs when they said nothing.
struct ModelArgs<'a> {
    provider: Option<&'a str>,
    backend: Option<BackendKind>,
    model: Option<&'a str>,
    ollama_url: Option<&'a str>,
    openai_url: Option<&'a str>,
    api_key_file: Option<&'a std::path::Path>,
    tool_calls: Option<provider::ToolCalls>,
    /// What this run budgets against, and 0 for "nothing said" — in which case
    /// the profile's own `context-limit` answers, because on the OpenAI API the
    /// window is a fact about the server and nowhere else.
    context_limit: u32,
    mock_delay_ms: u64,
    mock_replies: Vec<String>,
    mock_cycle: bool,
}

/// The destination, resolved, announced, and built.
///
/// One function for `serve`, `stdio` and `chat`, which is the point: three
/// callers assembling a backend out of the same flags is how the three drift
/// apart, and the file they now all read can only move them together. It hands
/// back the window as well, because a profile may have supplied it.
/// The `[engine.*]` a profile names, started and answering before a run sends
/// — what `serve` does per session, done once for `chat` and `stdio`. `None`
/// where the profile names none. The returned handle is the server's life:
/// dropping it stops what it started, and a server something else already runs
/// on that port is used and left alone. See
/// `RECORD/2026-09-28.a-model-server-luu-starts.completed.md`.
async fn engine_for(
    resolved: &provider::Resolved,
) -> Result<Option<std::sync::Arc<engines::Supervisor>>> {
    let Some((name, engine)) = engines::for_run(resolved) else {
        return Ok(None);
    };
    let supervisor = std::sync::Arc::new(engines::Supervisor::default());
    eprintln!("engine: {name} ({})", engine.kind.as_str());
    supervisor
        .ensure(&name, &engine)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok(Some(supervisor))
}

/// The window the server says it serves, where nothing declared one — asked
/// once, before the first turn, and said. A flag or a profile's
/// `context-limit` wins and nothing is asked. `serve` asks the same question
/// in the background per session instead; see `serve::learn_window` and
/// `RECORD/2026-09-30.the-window-the-server-says.completed.md`.
async fn with_server_window(
    mut resolved: provider::Resolved,
    backend: &dyn Backend,
) -> provider::Resolved {
    if resolved.context_limit > 0 {
        return resolved;
    }
    if let Ok(Some(window)) = backend.window().await {
        eprintln!("window: {window}, from the server");
        resolved.context_limit = window;
        resolved.window_from = provider::WindowFrom::Server;
    }
    resolved
}

fn destination(args: ModelArgs<'_>) -> Result<(Box<dyn Backend>, provider::Resolved)> {
    let (config, path) = provider::Config::load()?;
    let resolved = provider::resolve(
        &config,
        path.as_deref(),
        provider::Flags {
            provider: args.provider,
            backend: args.backend,
            model: args.model,
            ollama_url: args.ollama_url,
            openai_url: args.openai_url,
            api_key_file: args.api_key_file,
            context_limit: args.context_limit,
            tool_calls: args.tool_calls,
        },
    )?;
    // Before anything is sent, and on every run that has somewhere to send to.
    // A profile is a destination typed once and used for months, so this line is
    // what is left of *the destination having been typed*.
    if let Some(line) = resolved.line() {
        eprintln!("{line}");
    }

    let mock = match args.mock_replies.is_empty() {
        true => Mock::default(),
        false => Mock::replies(args.mock_replies),
    }
    .cycle(args.mock_cycle)
    .delay(Duration::from_millis(args.mock_delay_ms));
    let backend = build_backend(&resolved, Some(mock), true)?;
    // The mock's model name is the mock's, and it is written back here so
    // every reader of the resolution — the record header, the page — sees the
    // same string the turn will use.
    let mut resolved = resolved;
    resolved.model = model_for(backend.as_ref(), resolved.model);
    Ok((backend, resolved))
}

/// One resolution, built into the backend that will answer it.
///
/// Split out of [`destination`] because it is no longer only the CLI that
/// builds one: a session that names a provider builds another while the server
/// is running (`serve::destination_for`). What that caller does *not* have is a
/// terminal — hence `announce`, which is the difference between a run
/// starting and a request being served.
pub(crate) fn build_backend(
    resolved: &provider::Resolved,
    mock: Option<Mock>,
    announce: bool,
) -> Result<Box<dyn Backend>> {
    Ok(match resolved.kind {
        BackendKind::Mock => Box::new(mock.unwrap_or_default()),
        BackendKind::Ollama => Box::new(Ollama::new(&resolved.url)),
        BackendKind::Openai => {
            // Once, before anything is measured: this API has no field for the
            // window, so a run that budgets 8192 against a server started with
            // 4096 can only be told apart afterwards, by the prompt_tokens each
            // turn reports. Saying nothing here is how that becomes invisible.
            if announce && let Some(caveat) = OpenAi::window_caveat(Some(resolved.context_limit)) {
                eprintln!("note: {caveat}");
            }
            let mut backend = OpenAi::new(&resolved.url);
            if let Some(path) = &resolved.key_file {
                let key = crate::secret::read(
                    path,
                    "the API key file",
                    "hands the key to anyone who can read it here",
                )?;
                backend = backend.with_api_key(key);
            }
            Box::new(backend)
        }
    })
}

/// What a server building a destination mid-run wants: no fixtures, no delay,
/// and nothing printed to a terminal nobody is watching.
pub(crate) fn backend_for(resolved: &provider::Resolved) -> Result<Box<dyn Backend>> {
    build_backend(resolved, None, false)
}

/// The mock ignores the model name; passing the Ollama default through would
/// just put a model nobody loaded into the record file's header.
pub(crate) fn model_for(backend: &dyn Backend, model: String) -> String {
    match backend.name() {
        "mock" => "mock".to_string(),
        _ => model,
    }
}

/// One instruction from a script, or the single prompt of a one-shot `chat`.
///
/// A script is the only harness that makes two runs comparable, and a task is
/// confirmed before anything runs. Both hold: **the script carries the approved
/// plan**, written down and reviewable in a diff, and approving it is a check
/// rather than a question. See `RECORD/2026-08-30.tasks-in-code.completed.md` for why
/// this and not an `--auto-approve` flag.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    Prompt(String),
    /// `## fragment: <path>[:start-end]` — fuses a file into the next prompt.
    Fragment(String),
    OpenJob {
        objective: String,
        plan: Plan,
    },
    CloseJob,
}

/// Parses a script: prompts one per line, `#` comments, and `##` directives.
///
/// ```text
/// ## job: explain the context manager
/// ## task: read the design
/// ## file: luu-design.md
/// ## write: luu-design.md
/// ## fragment: luu-design.md:1-40
/// what does the context manager do?
/// ## close
/// ```
///
/// `## file:` and `## fragment:` are not the same thing and the names have to
/// stay apart: the first declares a path the *plan* is allowed to touch, the
/// second puts that file's text into the next prompt. A plan may name a file it
/// never reads, and a fragment may ground a turn no plan mentions.
///
/// A directive it does not know is an error rather than a prompt: a typo that
/// silently becomes a question would put a line of `## fille: x` into a
/// recorded baseline and nothing would ever say so.
fn parse_script(text: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut open = false;

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        let number = number + 1;
        if line.is_empty() || (line.starts_with('#') && !line.starts_with("##")) {
            continue;
        }
        let Some(directive) = line.strip_prefix("##").map(str::trim) else {
            steps.push(Step::Prompt(line.to_string()));
            continue;
        };

        let (key, value) = match directive.split_once(':') {
            Some((key, value)) => (key.trim(), value.trim()),
            None => (directive, ""),
        };

        match key {
            "job" => {
                if open {
                    anyhow::bail!("line {number}: a job is still open; `## close` it first");
                }
                if value.is_empty() {
                    anyhow::bail!("line {number}: `## job:` needs an objective");
                }
                open = true;
                steps.push(Step::OpenJob {
                    objective: value.to_string(),
                    plan: Plan::default(),
                });
            }
            "task" => {
                if !open {
                    if value.is_empty() {
                        anyhow::bail!("line {number}: `## task:` needs an objective");
                    }
                    open = true;
                    steps.push(Step::OpenJob {
                        objective: value.to_string(),
                        plan: Plan::default(),
                    });
                } else if let Some(Step::OpenJob { plan, .. }) = steps.last_mut() {
                    plan.tasks.push(value.to_string());
                } else {
                    anyhow::bail!("line {number}: a job is still open; `## close` it first");
                }
            }
            // The plan belongs to the job being opened and has to be the last
            // thing pushed: it is approved before its turns run, so it cannot
            // grow after one of them has.
            "step" | "file" | "write" | "command" | "network" | "egress" | "enforcement" => {
                let Some(Step::OpenJob { plan, .. }) = steps.last_mut() else {
                    anyhow::bail!(
                        "line {number}: `{line}` must follow a `## job:` (or `## task:`), before its first prompt"
                    );
                };
                match key {
                    "step" => plan.tasks.push(value.to_string()),
                    "file" => plan.files.push(value.to_string()),
                    // `## file:` is what the job may read; `## write:` what it
                    // may also change. A plan that declares no writes may not
                    // write — the check is worth having only if it can say no.
                    "write" => plan.writes.push(value.to_string()),
                    "command" => plan.commands.push(value.to_string()),
                    "network" => {
                        plan.network = match value {
                            "" | "true" | "yes" | "on" => true,
                            "false" | "no" | "off" => false,
                            other => anyhow::bail!(
                                "line {number}: `## network:` expects true or false, got `{other}`"
                            ),
                        };
                    }
                    // A plan may ask for *more* than the session here and
                    // not for less, which is the opposite of every other
                    // directive above — `unmet` is where that is refused, when
                    // the script is read and before any turn runs.
                    "enforcement" => {
                        plan.enforcement = Some(match value {
                            "kernel" => agent_core::sandbox::Enforcement::Kernel,
                            "best-effort" => agent_core::sandbox::Enforcement::BestEffort,
                            other => anyhow::bail!(
                                "line {number}: `## enforcement:` expects kernel or \
                                 best-effort, got `{other}`"
                            ),
                        });
                    }
                    "egress" => {
                        for domain in value
                            .split([',', ' '])
                            .map(str::trim)
                            .filter(|d| !d.is_empty())
                        {
                            plan.egress.push(domain.to_string());
                        }
                        plan.network = true;
                    }
                    _ => unreachable!(),
                }
            }
            // Attached to the next prompt, wherever it appears — inside a job
            // or not. It is grounding for one turn, not part of the plan.
            "fragment" => {
                if value.is_empty() {
                    anyhow::bail!("line {number}: `## fragment:` needs a path");
                }
                steps.push(Step::Fragment(value.to_string()));
            }
            "close" => {
                if !open {
                    anyhow::bail!("line {number}: `## close` with no job open");
                }
                open = false;
                steps.push(Step::CloseJob);
            }
            _ => anyhow::bail!(
                "line {number}: `{line}` is not a directive \
                 (`## job:`, `## task:`, `## step:`, `## file:`, `## write:`, \
                 `## command:`, `## network:`, `## egress:`, `## enforcement:`, \
                 `## fragment:`, `## close`)"
            ),
        }
    }

    if !steps.iter().any(|step| matches!(step, Step::Prompt(_))) {
        anyhow::bail!("no prompts in it");
    }
    Ok(steps)
}

/// Reads one `--fragment` or `## fragment:`, through the sandbox.
///
/// The user typed the path, not the model, so this is not the rule about model
/// output reaching the filesystem. It is the other one: a path `read_file`
/// would refuse must not become readable by spelling it in a flag.
///
/// A denial is an error rather than a warning. A run that quietly dropped the
/// file it was told to ground itself with would answer out of the model's
/// training and look like it worked, which is the failure this surface exists
/// to remove.
fn load_fragment(sandbox: &Sandbox, spec: &str) -> Result<Fragment> {
    fragment::load(sandbox, &fragment::Spec::parse(spec))
        .with_context(|| format!("fragment {spec}"))
}

/// A turn's code as the wire carries it: the specs, and who chose each one.
///
/// `by_hand` is where the person's fragments end and the selector's spans
/// begin — the two live in one `Vec<Fragment>` from the moment they are joined
/// and nothing downstream can take them apart, so the boundary has to be
/// handed in rather than inferred. See
/// `RECORD/2026-09-19.fragments-by-reference.completed.md` §Two things.
fn grounded_spans(code: &[Fragment], by_hand: usize) -> Vec<protocol::Grounding> {
    code.iter()
        .enumerate()
        .map(|(at, fragment)| protocol::Grounding {
            spec: fragment.path.clone(),
            origin: match at < by_hand {
                true => protocol::Origin::Attached,
                false => protocol::Origin::Selected,
            },
        })
        .collect()
}

/// A tally, for a person.
///
/// Every rate is *of renders*, and every line that has a token total names the
/// counter that produced it — a total over a session resumed onto another model
/// mixes two measurements, and this is the surface that has to say so rather
/// than the one that gets to assume.
///
/// A rule that never fired prints as a line saying zero rather than as no line
/// at all. The two are different claims about a session and the difference is
/// the whole reason the window's rules carry `None` semantics in the header:
/// *off* and *on and quiet* are not the same run.
fn describe_counts(view: &SessionView, counts: &Counts) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} — {} / {}, {} turn(s)",
        view.id, view.backend, view.model, counts.turns,
    );
    let _ = writeln!(
        out,
        "rules: repeat = {}, prune = {}, results = {}",
        rule_name(view.repeat.as_ref()),
        rule_name(view.prune.as_ref()),
        rule_name(view.results.as_ref()),
    );

    let _ = writeln!(
        out,
        "\nwindow, of {} render(s)",
        counts
            .turns
            .max(counts.diverged.renders)
            .max(counts.superseded.renders),
    );
    let _ = writeln!(
        out,
        "  repeated   {:>3} render(s)  {:>3} turn(s), {} asking, {} span(s){}",
        counts.repeated.renders,
        counts.repeated.turns,
        counts.repeated.asking,
        counts.repeated.spans,
        tokens_suffix(&counts.repeated.tokens),
    );
    let _ = writeln!(
        out,
        "  pruned     {:>3} render(s)  {:>3} turn(s) gave up spans{}",
        counts.pruned.renders,
        counts.pruned.turns,
        tokens_suffix(&counts.pruned.tokens),
    );
    let _ = writeln!(
        out,
        "  evicted    {:>3} render(s)  {:>3} turn(s) left{}",
        counts.evicted.renders,
        counts.evicted.turns,
        tokens_suffix(&counts.evicted.tokens),
    );
    let _ = writeln!(
        out,
        "  diverged   {:>3} render(s)  {:>3} asking, {} line(s), {} path(s)",
        counts.diverged.renders,
        counts.diverged.asking,
        counts.diverged.lines,
        counts.diverged.paths.len(),
    );
    // Named rather than counted, on the tombstone's rule: a reader months later
    // cannot recover *which* path from a number, and there are never many.
    for (path, bodies) in &counts.diverged.paths {
        let _ = writeln!(out, "               {path}  up to {bodies} bodies");
    }
    let _ = writeln!(
        out,
        "  superseded {:>3} render(s)  {:>3} line(s), {} span(s) replaced{}",
        counts.superseded.renders,
        counts.superseded.lines,
        counts.superseded.spans,
        tokens_suffix(&counts.superseded.tokens),
    );
    for (path, spans) in &counts.superseded.paths {
        let _ = writeln!(out, "               {path}  {spans} span(s)");
    }

    let _ = writeln!(
        out,
        "\ntools: {} call(s), {} allowed, {} denied, {} undecided",
        counts.calls.total, counts.calls.allowed, counts.calls.denied, counts.calls.undecided,
    );
    for rule in &counts.calls.rules {
        let _ = writeln!(
            out,
            "  {:<24} {} allowed, {} denied",
            rule.rule, rule.allowed, rule.denied,
        );
    }

    let _ = writeln!(
        out,
        "\njobs: {} proposed, {} approved, {} rejected, {} closed",
        counts.jobs.proposed, counts.jobs.approved, counts.jobs.declined, counts.jobs.closed,
    );
    // "path(s)" and not "file(s)", deliberately: which of `src/` and
    // `src/lib.rs` is a directory is a question about a disk that a fold has no
    // business asking. Item 12 of ROADMAP/2026-09-17 is taken this far and no
    // further.
    let _ = writeln!(
        out,
        "  granted: {} readable path(s), {} writable",
        counts.jobs.reads, counts.jobs.writes,
    );

    match counts.refused.is_empty() {
        true => {
            let _ = writeln!(out, "\nrefused: nothing");
        }
        false => {
            let _ = writeln!(out, "\nrefused");
            for refusal in &counts.refused {
                let _ = writeln!(
                    out,
                    "  {:<12} {}",
                    wire_name(&refusal.reason),
                    refusal.count,
                );
            }
        }
    }

    out
}

/// The counter beside a total, or nothing when there is no total to qualify.
///
/// Two counters on one line is not a formatting accident: it means the session
/// was resumed onto another model and the sum mixes measurements, which is the
/// one thing a reader must not have to work out for themselves.
fn tokens_suffix(tokens: &Tokens) -> String {
    match tokens.counters.as_slice() {
        [] => String::new(),
        [one] => format!(", {} tokens by {}", tokens.tokens, counter_name(one)),
        many => format!(
            ", {} tokens mixing {}",
            tokens.tokens,
            many.iter()
                .map(counter_name)
                .collect::<Vec<_>>()
                .join(" + "),
        ),
    }
}

/// A rule as the header names it, or `?` where the header does not name it.
///
/// `?` and not the default, which is the header's own `None` semantics: a
/// stream written before format 11 does not say what it ran under, every one of
/// those ran under the defaults, and reading them back for it would put a claim
/// in a record the record never made.
fn rule_name<T: serde::Serialize>(rule: Option<&T>) -> String {
    rule.map(wire_name).unwrap_or_else(|| "?".to_string())
}

/// A tagged enum's own wire spelling, which is the one the header, the page and
/// the flags all already use. Naming them a second time in a `match` here is
/// how two spellings for one value get into a tree.
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

fn counter_name(counter: &Counter) -> String {
    match counter {
        Counter::Approximate => "approximate".to_string(),
        Counter::Model { id } => id.clone(),
    }
}

pub async fn run() -> Result<()> {
    let Cli { command } = Cli::parse();

    if let Command::Export {
        records,
        out,
        record_base,
    } = &command
    {
        let sessions = records
            .iter()
            .map(|path| {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .context("a record file needs a name")?
                    .to_string();
                let url = format!(
                    "{}/{}",
                    record_base.trim_end_matches('/'),
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                );
                Ok(export::Session {
                    id,
                    record: export::RecordSource { url },
                    lines: export::read_record(path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let summaries = export::export(&sessions, out)?;
        for summary in &summaries {
            println!("{} — {} turn(s)", summary.id, summary.turns);
        }
        println!("written to {}", out.display());
        return Ok(());
    }

    if let Command::Count { record, json } = &command {
        let id = record
            .file_stem()
            .and_then(|s| s.to_str())
            .context("a record file needs a name")?;
        // Through the fold and not over the lines: the tally is arithmetic over
        // what a reader of the session can see, and the fold is the structure
        // both the live server and `luu export` already read it through.
        let view = SessionView::from_record(id, &export::read_record(record)?);
        let counts = view.counts();
        match json {
            true => println!("{}", serde_json::to_string_pretty(&counts)?),
            false => print!("{}", describe_counts(&view, &counts)),
        }
        return Ok(());
    }

    // Before everything else, and reading no policy file: the worker is handed
    // its sandbox with every call, because the paths a host resolved are not
    // the paths this side has.
    if let Command::Worker { commands } = &command {
        return serve_stdio(
            std::sync::Arc::new(Tools::standard()),
            commands.clone(),
            tokio::io::stdin(),
            tokio::io::stdout(),
        )
        .await
        .context("the worker");
    }

    if let Command::Key { action } = &command {
        return run_key(action);
    }

    if let Command::Tools { sandbox } = &command {
        let agency = sandbox.resolve().await?;
        print!("{}", agency.describe());
        let definitions = agency.definitions();
        if !definitions.is_empty() {
            println!("\n--- the prefix block, verbatim ---\n{definitions}");
        }
        return Ok(());
    }

    if let Command::Select {
        query,
        sandbox,
        select_tokens,
        select_docs,
        select_graph,
        tokenizer,
        explain,
    } = &command
    {
        let agency = sandbox.resolve().await?;
        let (counter, warning) = counter_for("the selection", tokenizer.as_deref())?;
        if let Some(warning) = &warning {
            eprintln!("warning: {warning}");
        }
        let walked = agent_core::repo_map::walk_sources(agency.sandbox.as_ref());
        let selection = agent_core::select::select(
            &walked,
            agency.sandbox.as_ref(),
            query,
            *select_tokens,
            counter.as_ref(),
            &weights_of(*select_docs, *select_graph),
        );
        print!("{}", selection.render(*explain));
        return Ok(());
    }

    if let Command::Map {
        sandbox,
        map_tokens,
        map_rank,
        map_in_degree,
        map_non_greedy,
        tokenizer,
        explain,
    } = &command
    {
        let agency = sandbox.resolve().await?;
        // Named, because the warning that comes back says which model the
        // count belongs to — and a map counted by `chars/4` is a different
        // number from the one a run with a tokenizer would spend.
        let (counter, warning) = counter_for("the map", tokenizer.as_deref())?;
        if let Some(warning) = &warning {
            eprintln!("warning: {warning}");
        }
        let order = order_of(*map_rank, *map_in_degree);
        let fill = if *map_non_greedy {
            agent_core::repo_map::Fill::NonGreedy
        } else {
            agent_core::repo_map::Fill::Greedy
        };
        let map = RepoMap::build_with(
            agency.sandbox.as_ref(),
            *map_tokens,
            counter.as_ref(),
            order,
            fill,
        );
        print!("{}", map.render());
        if *explain {
            println!("\n--- the ranking, most depended-on first ---");
            for file in &map.ranked {
                let why = match file.referrers.is_empty() {
                    // Named rather than left blank: a file at the top with
                    // nothing pointing at it is the ranking admitting it had
                    // nothing to go on, which is worth seeing.
                    true => "referenced by nothing the sandbox can read".to_string(),
                    false => file
                        .referrers
                        .iter()
                        .take(3)
                        .map(|(path, weight)| format!("{path} ({weight:.2})"))
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                println!(
                    "{} {:.5}  {}\n         {why}",
                    match file.in_map {
                        true => "in ",
                        false => "out",
                    },
                    file.score,
                    file.path,
                );
            }
        }
        println!(
            "\n--- {} file(s) outlined, {} left out, {} of {map_tokens} tokens{} ---",
            map.files.len(),
            map.left_out,
            map.tokens,
            match map.counted_by.is_approximate() {
                true => " (approximate: pass --tokenizer)",
                false => "",
            },
        );
        return Ok(());
    }

    if let Command::Serve {
        bind,
        auth_token_file,
        store,
        no_store,
        no_log,
        sandbox_args,
        provider,
        backend,
        model,
        ollama_url,
        openai_url,
        api_key_file,
        tool_calls,
        mock_delay_ms,
        mock_replies,
        mock_cycle,
        record,
        context_limit,
        tokenizer,
        reserve,
        evict,
        low_water,
        repeat_once,
        no_repeat_once,
        prune_behind,
        prune_results,
        temperature,
        seed,
        map_tokens,
        map_rank,
        map_in_degree,
        map_non_greedy,
        select_tokens,
        select_docs,
        select_graph,
        constrain,
    } = command
    {
        // First, so everything after it — the engine starting, the listener
        // binding — is in it. Held for the life of `serve`: dropping it is what
        // flushes the writer's thread. See `crate::log`.
        let _log = match (no_log, crate::config::dir()) {
            (true, _) => None,
            (false, None) => {
                eprintln!("log — none: there is no state directory to keep one in");
                None
            }
            (false, Some(dir)) => match crate::log::open(&dir) {
                Ok(log) => {
                    eprintln!("log — {}/serve.<date>.log", log.dir.display());
                    tracing::info!(target: "luu::serve", version = env!("CARGO_PKG_VERSION"), "serve starting");
                    Some(log)
                }
                Err(error) => {
                    eprintln!("warning: no log: {error:#}");
                    None
                }
            },
        };
        let (backend, resolved) = destination(ModelArgs {
            provider: provider.as_deref(),
            backend,
            model: model.as_deref(),
            ollama_url: ollama_url.as_deref(),
            openai_url: openai_url.as_deref(),
            api_key_file: api_key_file.as_deref(),
            tool_calls,
            context_limit,
            mock_delay_ms,
            mock_replies,
            mock_cycle,
        })?;
        let model = resolved.model.clone();
        let context_limit = resolved.context_limit;
        let (counter, warning) = counter_for(&model, tokenizer.as_deref())?;
        if let Some(warning) = &warning {
            eprintln!("warning: {warning}");
        }
        let approvers = sandbox_args.approvers()?;
        let agency = sandbox_args.resolve().await?;
        // The postures this machine names, beside the providers and out of the
        // same file. A config that will not load is not a reason to refuse to
        // serve: it is already reported where the providers are, and a machine
        // with none simply offers none.
        let (postures, postures_path, ui, resend) = match crate::provider::Config::load() {
            Ok((config, path)) => (
                config.postures().clone(),
                path.map(|path| path.display().to_string()),
                config.ui().cloned(),
                config.resend().unwrap_or_default(),
            ),
            Err(_) => (Default::default(), None, None, Default::default()),
        };
        // Read once, here, so a theme that will not load says so beside the
        // other startup lines rather than as a silent absence of icons in the
        // page. A failure is not fatal: the tree draws its own shapes.
        let icons = match ui.as_ref().and_then(|ui| ui.icon_theme.as_ref()) {
            None => std::sync::Arc::new(crate::icons::Theme::default()),
            Some(named) => {
                let path = crate::provider::expand_home(named);
                match crate::icons::load(&path) {
                    Ok(theme) => {
                        eprintln!(
                            "icons — {} ({} file types)",
                            theme.manifest.name.as_deref().unwrap_or("icon theme"),
                            theme.manifest.file_extensions.len() + theme.manifest.file_names.len(),
                        );
                        std::sync::Arc::new(theme)
                    }
                    Err(error) => {
                        eprintln!("warning: [ui] icon-theme did not load: {error}");
                        std::sync::Arc::new(crate::icons::Theme::default())
                    }
                }
            }
        };
        // The grammars, compiled here rather than inside the first click: the
        // queries are the same every time and building them per request was
        // most of what opening a file cost. Beside the icon theme because it is
        // the same kind of decision — pay a fixed cost where somebody is
        // watching the process start. See the phase 8 section of
        // `RECORD/2026-09-15.a-three-pane-inspector.completed.md`.
        crate::highlight::warm();
        eprint!("{}", agency.describe());
        if approvers.required {
            eprintln!(
                "approvals — signed, {} key(s) named in the policy file",
                approvers.keys.len()
            );
        }
        // Named, or the default, or nothing at all. A missing `HOME` leaves
        // the default undecidable, and the run says so rather than picking a
        // directory of its own and writing history into it.
        let store = match (no_store, store) {
            (true, _) => None,
            (false, Some(path)) => Some(path),
            (false, None) => {
                let path = crate::store::default_path();
                if path.is_none() {
                    eprintln!(
                        "warning: no state directory (neither LUU_HOME nor HOME is set), so no \
                         default session store: this session stays in memory. \
                         Pass --store <path> to keep it."
                    );
                }
                path
            }
        };
        return serve::serve(serve::ServeOptions {
            icons,
            provider: resolved,
            counter_warning: warning,
            address: bind,
            backend: backend.into(),
            model,
            record,
            // Read from the same config load the postures came from, and not
            // a second one: what a run starts under should not change under the
            // surface that is about to offer it.
            budget: budget_under(
                Budget::new(context_limit, reserve, evict.policy(low_water)),
                resend_rules(
                    resend,
                    repeat_asked(repeat_once, no_repeat_once),
                    prune_behind,
                    prune_results,
                ),
            ),
            counter,
            tokenizer,
            agency,
            // How a session's posture is built, as a function: the same
            // flags with one field replaced, so every other one means what it
            // meant. Only `serve` gets it — over stdio the process is the
            // session, and its policy file is a flag.
            // Read once, here: what a session may choose should not change
            // under the surface that is offering it, which is the rule the
            // modal's first section states about everything else this run
            // resolved. A posture added to `config.toml` is offered by the
            // next run.
            postures: postures.clone(),
            postures_path: postures_path.clone(),
            describe_for: Some({
                let args = sandbox_args.clone();
                std::sync::Arc::new(
                    move |policy: Option<std::path::PathBuf>,
                          runtime: Option<agent_core::worker::Runtime>| {
                        args.describe_posture(policy.as_deref(), runtime)
                    },
                )
            }),
            agency_for: Some({
                let args = sandbox_args.clone();
                std::sync::Arc::new(
                    move |policy: Option<std::path::PathBuf>,
                          runtime: Option<agent_core::worker::Runtime>| {
                        let args = args.clone();
                        Box::pin(
                            async move { args.resolve_posture(policy.as_deref(), runtime).await },
                        )
                            as futures_util::future::BoxFuture<'static, Result<_>>
                    },
                )
            }),
            temperature,
            seed,
            store,
            map_tokens,
            map_order: order_of(map_rank, map_in_degree),
            map_fill: fill_of(map_non_greedy),
            select_tokens,
            select_weights: weights_of(select_docs, select_graph),
            constrain,
            auth_token_file,
            approvers,
        })
        .await;
    }

    if let Command::Stdio {
        store,
        no_store,
        sandbox_args,
        provider,
        backend,
        model,
        ollama_url,
        openai_url,
        api_key_file,
        tool_calls,
        mock_delay_ms,
        mock_replies,
        mock_cycle,
        record,
        context_limit,
        tokenizer,
        reserve,
        evict,
        low_water,
        repeat_once,
        no_repeat_once,
        prune_behind,
        prune_results,
        temperature,
        seed,
        map_tokens,
        map_rank,
        map_in_degree,
        map_non_greedy,
        select_tokens,
        select_docs,
        select_graph,
        constrain,
    } = command
    {
        let (backend, resolved) = destination(ModelArgs {
            provider: provider.as_deref(),
            backend,
            model: model.as_deref(),
            ollama_url: ollama_url.as_deref(),
            openai_url: openai_url.as_deref(),
            api_key_file: api_key_file.as_deref(),
            tool_calls,
            context_limit,
            mock_delay_ms,
            mock_replies,
            mock_cycle,
        })?;
        // Held for as long as the protocol runs, and stopped with it.
        let _engine = engine_for(&resolved).await?;
        let resolved = with_server_window(resolved, backend.as_ref()).await;
        let model = resolved.model.clone();
        let context_limit = resolved.context_limit;
        let (counter, warning) = counter_for(&model, tokenizer.as_deref())?;
        if let Some(warning) = &warning {
            eprintln!("warning: {warning}");
        }
        let approvers = sandbox_args.approvers()?;
        let agency = sandbox_args.resolve().await?;
        eprint!("{}", agency.describe());
        let store = match (no_store, store) {
            (true, _) => None,
            (false, Some(path)) => Some(path),
            (false, None) => {
                let path = crate::store::default_path();
                if path.is_none() {
                    eprintln!(
                        "warning: no state directory (neither LUU_HOME nor HOME is set), so no \
                         default session store: this session stays in memory. \
                         Pass --store <path> to keep it."
                    );
                }
                path
            }
        };
        return serve::stdio(serve::StdioOptions {
            // No page over stdio, so no icons to serve one.
            icons: std::sync::Arc::new(crate::icons::Theme::default()),
            provider: resolved,
            counter_warning: warning,
            backend: backend.into(),
            model,
            record,
            budget: budget_under(
                Budget::new(context_limit, reserve, evict.policy(low_water)),
                resend_rules(
                    machine_resend(),
                    repeat_asked(repeat_once, no_repeat_once),
                    prune_behind,
                    prune_results,
                ),
            ),
            counter,
            tokenizer,
            agency,
            // `None`: over stdio the process *is* the session, so its policy
            // file is a flag and a session has nothing to choose.
            agency_for: None,
            describe_for: None,
            postures: Default::default(),
            postures_path: None,
            temperature,
            seed,
            store,
            map_tokens,
            map_order: order_of(map_rank, map_in_degree),
            map_fill: fill_of(map_non_greedy),
            select_tokens,
            select_weights: weights_of(select_docs, select_graph),
            constrain,
            approvers,
        })
        .await;
    }

    let Command::Chat {
        prompt,
        sandbox_args,
        script,
        fragments,
        provider,
        backend,
        model,
        ollama_url,
        openai_url,
        api_key_file,
        tool_calls,
        mock_delay_ms,
        mock_replies,
        mock_cycle,
        cancel_after_ms,
        record,
        context_limit,
        tokenizer,
        reserve,
        evict,
        low_water,
        repeat_once,
        no_repeat_once,
        prune_behind,
        prune_results,
        temperature,
        seed,
        map_tokens,
        map_rank,
        map_in_degree,
        map_non_greedy,
        select_tokens,
        select_docs,
        select_graph,
        constrain,
    } = command
    else {
        unreachable!("serve and tools are handled above");
    };

    let agency = sandbox_args.resolve().await?;

    // One prompt, or a file of them: a script is what makes a multi-turn run
    // repeatable, and a baseline that cannot be re-run is not a baseline.
    let steps = match (&script, prompt) {
        (Some(path), _) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            parse_script(&text).with_context(|| path.display().to_string())?
        }
        (None, Some(prompt)) => vec![Step::Prompt(prompt)],
        (None, None) => vec![Step::Prompt(std::io::read_to_string(std::io::stdin())?)],
    };

    let (backend, resolved) = destination(ModelArgs {
        provider: provider.as_deref(),
        backend,
        model: model.as_deref(),
        ollama_url: ollama_url.as_deref(),
        openai_url: openai_url.as_deref(),
        api_key_file: api_key_file.as_deref(),
        tool_calls,
        context_limit,
        mock_delay_ms,
        mock_replies,
        mock_cycle,
    })?;
    // Held until the run ends: dropping it stops the server it started.
    let _engine = engine_for(&resolved).await?;
    let resolved = with_server_window(resolved, backend.as_ref()).await;
    let model = resolved.model.clone();
    let context_limit = resolved.context_limit;
    let (counter, warning) = counter_for(&model, tokenizer.as_deref())?;
    if let Some(warning) = &warning {
        eprintln!("warning: {warning}");
    }

    let budget = budget_under(
        Budget::new(context_limit, reserve, evict.policy(low_water)),
        resend_rules(
            machine_resend(),
            repeat_asked(repeat_once, no_repeat_once),
            prune_behind,
            prune_results,
        ),
    );
    let started_at = now_ms();
    let recorder = match &record {
        Some(path) => Some(std::sync::Arc::new(
            Recorder::create(
                path,
                backend.name(),
                &model,
                budget,
                counter.id(),
                // A one-shot names no posture: its policy file is the flag it
                // was given, and the three facts are what that resolved to.
                Some(agency.posture(None)),
                // `luu chat` has no `Authority::Draft` to narrow — every job a
                // script opens is pre-approved by the script itself — so
                // there is nothing here to tell a model about. See
                // `RECORD/2026-09-22.an-authority-a-model-is-told.completed.md`.
                &crate::provider::AuthorityNotes::default(),
                resolved.tool_calls,
                started_at,
            )
            .await?,
        )),
        None => None,
    };

    // The history the turns accumulate into. One turn and it stays empty; the
    // selection still runs either way, because a `chat` that assembled its
    // prompt differently from `serve` would be measuring something the server
    // never sends.
    // The map is built once, before the first turn: it is the last block of the
    // cached prefix, and a block that is rebuilt mid-session is not a prefix.
    // What that costs — an agent that edits a file then carries the outline it
    // had — is named in `RECORD/2026-08-31.the-repo-map.completed.md`.
    let map = RepoMap::build_with(
        agency.sandbox.as_ref(),
        map_tokens,
        counter.as_ref(),
        order_of(map_rank, map_in_degree),
        fill_of(map_non_greedy),
    );
    if !map.is_empty() {
        println!(
            "== repository map: {} file(s), {} left out, {} of {map_tokens} tokens",
            map.files.len(),
            map.left_out,
            map.tokens,
        );
    }
    let tool_calls = resolved.tool_calls;
    let mut context = agency
        .with_tools(AgentContext::new(SYSTEM), tool_calls)
        .with_map(map.render());
    // Built once, before the first turn — a constraint compiled per call
    // would cost the compile every turn for a value that is the same every
    // time. `constrain_caveat` is asked here too, once, so a run that sent
    // one is told what the backend could not promise about it before
    // anything is measured, the same rule the window caveat set.
    let built = constrain
        .map(|kind| kind.build(&agency.tools))
        .transpose()?;
    if let Some(built) = &built
        && let Some(caveat) = backend.constrain_caveat(built)
    {
        eprintln!("note: {caveat}");
    }
    // The two arms diverge here. `Grammar` is honestly a first-attempt
    // constraint — prose stays reachable through the alternation — so it is
    // sent on every step below, unchanged from before. `Schema` has no
    // "just answer" branch (`Tools::call_schema`'s own doc), so it is never
    // the per-step constraint; it is offered to `run_agent_turn` as a
    // `SchemaRetry`, spent only on a reply that drifted. See
    // `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.
    let (constraint, schema_retry) = match (constrain, built) {
        (Some(ConstrainKind::Grammar), Some(grammar)) => (Some(grammar), None),
        (Some(ConstrainKind::Schema), Some(Constraint::Schema(schema))) => (
            None,
            Some(SchemaRetry {
                schema,
                tool_names: agency.tools.names().collect(),
            }),
        ),
        _ => (None, None),
    };
    // Shared with the printer task, because the tool round trips are announced
    // there and they belong in the same chain as the turns: two trackers would
    // measure one session against two different pasts.
    let prefix = std::sync::Arc::new(std::sync::Mutex::new(PrefixTracker::default()));
    let mut failed = false;

    let multi = steps
        .iter()
        .filter(|step| matches!(step, Step::Prompt(_)))
        .count()
        > 1;
    let mut turn: agent_core::protocol::TurnId = 0;

    // Waiting to be fused into the next prompt, then gone: a fragment belongs
    // to one turn and is stored with it. The flags are read before the first
    // step, so `--fragment` and a one-shot prompt behave like a script that
    // opens with `## fragment:`.
    let mut attached: Vec<Fragment> = fragments
        .iter()
        .map(|spec| load_fragment(agency.sandbox.as_ref(), spec))
        .collect::<Result<_>>()?;

    // One walk of the tree, re-stamped by every turn. The sentence that used to
    // be here — "the tree does not change between two turns of one run" — is
    // false the moment a turn calls `edit_file`, and the cost of it being false
    // is not a missing file: the walk's line numbers go stale while the paths
    // stay valid, so the selector hands the next turn the *wrong lines* of the
    // right file. `rewalk_sources` re-parses only what moved.
    let mut walked = match select_tokens > 0 {
        true => Some(agent_core::repo_map::walk_sources(agency.sandbox.as_ref())),
        false => None,
    };

    // The live task's own sandbox, from `## task:` to `## close`. A script's
    // written plan is its approval, so it narrows exactly as a plan approved at
    // the gate does: what the task may touch is what the plan named.
    let mut narrowed: Option<std::sync::Arc<agent_core::sandbox::Sandbox>> = None;

    for step in &steps {
        // The task lifecycle happens between turns, and every part of it is
        // recorded: a run that cannot say what it was allowed to do is not a
        // baseline either.
        let prompt = match step {
            Step::OpenJob { objective, plan } => {
                // The confirmation, as a check rather than a question. A plan
                // that asks for what the sandbox does not grant does not run —
                // the alternative is discovering it as a denial four turns in.
                let unmet = plan.unmet(agency.sandbox.as_ref());
                if !unmet.is_empty() {
                    anyhow::bail!(
                        "job `{objective}` asks for what the sandbox does not grant:\n  {}",
                        unmet.join("\n  "),
                    );
                }
                // `luu chat` has no gate, so it has no approver either: the
                // policy file is its standing approval and the operator who
                // ran the command is who that was. See the open question in
                // `RECORD/2026-09-04.signed-approvals.completed.md`.
                //
                // One call and not two, because approving *is* opening: a
                // script's `## task:` closes whatever draft its earlier prompts
                // opened and starts the job the directive describes, which is
                // the same transition the gate makes.
                let approved = context.approve_plan(
                    objective.clone(),
                    plan.clone(),
                    plan.clone(),
                    PlanSource::Written,
                    ApprovedBy::Operator,
                    counter.as_ref(),
                );
                let id = approved.job;
                narrowed = Some(std::sync::Arc::new(
                    plan.narrow(agency.sandbox.as_ref(), id)
                        .with_context(|| format!("resolving the plan of job `{objective}`"))?,
                ));
                if let Some(recorder) = &recorder {
                    recorder.write(&Event::Protocol(ServerMessage::PlanProposed {
                        objective: objective.clone(),
                        plan: plan.clone(),
                        // No planning call happened: a script carries its plan,
                        // which is an approval written down in advance.
                        source: Some(PlanSource::Written),
                    }));
                    if let Some((draft, summary)) = &approved.folded {
                        recorder.write(&Event::Protocol(ServerMessage::JobClosed {
                            job: *draft,
                            summary: summary.clone(),
                            by: Some(ClosedBy::User),
                            replaced: context.replaced_by(*draft),
                        }));
                    }
                    recorder.write(&Event::Protocol(ServerMessage::JobApproved {
                        job: id,
                        from: approved.folded.as_ref().map(|(draft, _)| *draft),
                        objective: objective.clone(),
                        plan: plan.clone(),
                        approved_by: Some(ApprovedBy::Operator),
                    }));
                }
                println!("\n== job {id} approved: {objective}");
                print!("{}", plan.describe());
                continue;
            }
            Step::CloseJob => {
                // The parser guarantees a job is open here.
                let Some(id) = context.live_job() else {
                    unreachable!("`## close` with no job open is refused when the script is read")
                };
                let summary = context.close_job(id, counter.as_ref()).unwrap_or_default();
                // Outside a job the policy file is the whole answer again.
                narrowed = None;
                if let Some(recorder) = &recorder {
                    recorder.write(&Event::Protocol(ServerMessage::JobClosed {
                        job: id,
                        summary: summary.clone(),
                        // `## close` is a person's instruction written down in
                        // advance, which is the same authority typed later.
                        by: Some(ClosedBy::User),
                        replaced: context.replaced_by(id),
                    }));
                }
                println!("\n== job {id} closed; its turns are now sent as:");
                for line in summary.lines() {
                    println!("   {line}");
                }
                continue;
            }
            Step::Fragment(spec) => {
                // Through the sandbox that holds *now*: a path `read_file`
                // would refuse must not become readable by spelling it in a
                // directive, and inside a job it is the plan that refuses.
                let sandbox = narrowed.clone().unwrap_or_else(|| agency.sandbox.clone());
                let fragment = load_fragment(sandbox.as_ref(), spec)?;
                println!(
                    "\n== fragment {} — {} bytes into the next prompt",
                    fragment.path,
                    fragment.text.len()
                );
                attached.push(fragment);
                continue;
            }
            Step::Prompt(prompt) => prompt,
        };

        turn += 1;
        let job = context.live_job();
        // Taken, not copied: these fragments are this turn's, and the next turn
        // starts with none.
        let mut code = std::mem::take(&mut attached);
        // Where the person's end and the selector's begin. The two are one
        // `Vec<Fragment>` from here on and nothing downstream can take them
        // apart, which is the finding
        // `RECORD/2026-09-19.fragments-by-reference.completed.md` §Two things
        // records — so the boundary is caught here, at the one moment it is
        // still known, and carried on the wire.
        let by_hand = code.len();
        // And the ones nobody typed: what this turn's own text points at, on
        // top of what was attached by hand. The person's `--fragment` goes
        // first and keeps its whole budget — a selector that could crowd out an
        // explicit ask would be answering a question it was not asked.
        if let Some(walked) = &mut walked {
            // Re-stamped first, and through the **agency's** sandbox rather
            // than the live job's: the walk is the run's cache, and refreshing
            // it through a narrow plan would shrink it for every turn after.
            // Off the async thread, like every other filesystem call in a turn,
            // so the deadline above it can still fire.
            let previous = std::mem::take(walked);
            let agency_sandbox = agency.sandbox.clone();
            *walked = match tokio::task::spawn_blocking(move || {
                agent_core::repo_map::rewalk_sources(agency_sandbox.as_ref(), &previous)
            })
            .await
            {
                Ok(rewalked) => rewalked.walked,
                Err(error) => std::panic::resume_unwind(error.into_panic()),
            };
            // The live job's sandbox when there is one: a selection is read
            // through whatever the turn itself may read, so an approved plan
            // narrows what can be chosen exactly as it narrows what can be
            // opened.
            let sandbox = narrowed.clone().unwrap_or_else(|| agency.sandbox.clone());
            let selected = agent_core::select::select(
                walked,
                sandbox.as_ref(),
                prompt,
                select_tokens,
                counter.as_ref(),
                &weights_of(select_docs, select_graph),
            );
            // A selected path the sandbox refuses is a file that is not
            // selected, not a failed run: nobody asked for it by name.
            code.extend(
                selected
                    .specs()
                    .iter()
                    .filter_map(|spec| fragment::load(sandbox.as_ref(), spec).ok()),
            );
        }
        let selection = context.select(prompt, &code, budget, counter.as_ref());

        // Said out loud, not only into the recording: a run that quietly
        // forgets half its history looks exactly like one that answers from all
        // of it, and the difference is the whole subject.
        if let Some(evicted) = &selection.eviction {
            let turns = evicted
                .turns
                .iter()
                .map(|turn| turn.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "\n== evicted turn{} {turns} — {} tokens{} out of the window, for good",
                match evicted.turns.len() {
                    1 => "",
                    _ => "s",
                },
                evicted.tokens,
                match evicted.counter.is_approximate() {
                    true => " (approximate)",
                    false => "",
                },
            );
        }

        if let Some(recorder) = &recorder {
            recorder.write(&Event::Protocol(ServerMessage::TurnStarted {
                turn,
                prompt: prompt.clone(),
                job,
            }));
            // What it was asked *with*, by reference, straight after what was
            // asked — the two halves of the same question, and the only thing
            // a resume can read again. The order is the order they are fused
            // into the message, so the person's come first.
            if !code.is_empty() {
                recorder.write(&Event::Protocol(ServerMessage::Grounded {
                    turn,
                    spans: grounded_spans(&code, by_hand),
                }));
            }
            // Before the prompt it explains, so a file reads in the order the
            // session happened: the history was cut, then this is what was
            // sent.
            if let Some(evicted) = selection.eviction.clone() {
                recorder.write(&Event::Protocol(ServerMessage::Evicted {
                    turn,
                    turns: evicted.turns,
                    tokens: evicted.tokens,
                    counter: evicted.counter,
                    policy: evicted.policy,
                }));
            }
            let text = rendered(&selection.messages);
            let tool_specs = context.native_tools().map(str::to_string);
            let reuse = prefix
                .lock()
                .expect("the prefix tracker is never held across an await")
                .measure(turn, &text, counter.as_ref());
            recorder.write(&Event::Trace(TraceMessage::Prompt {
                turn,
                text,
                tool_specs,
            }));
            if let Some(reuse) = reuse {
                recorder.write(&Event::Trace(reuse));
            }
            // What the window took out of the prompt without taking the turn
            // with it. Beside the budget rather than beside the eviction: the
            // buckets are the only other place a reader learns what the prompt
            // was, as opposed to what the session is.
            if let Some(pruned) = selection.pruning.clone() {
                recorder.write(&Event::Trace(TraceMessage::Pruned {
                    turn,
                    turns: pruned.turns,
                    tokens: pruned.tokens,
                    counter: pruned.counter,
                }));
            }
            // Beside the prune it is the counterpart of, and before the
            // divergence, because a render that collapsed spans is what the
            // contradiction below is a contradiction *within*.
            if let Some(repeated) = selection.repeating.clone() {
                recorder.write(&Event::Trace(TraceMessage::Repeated {
                    turn,
                    turns: repeated.turns,
                    asking: repeated.asking,
                    spans: repeated.spans,
                    tokens: repeated.tokens,
                    counter: repeated.counter,
                }));
            }
            // One line per path, for the reason `serve` emits them the same
            // way: each is its own finding.
            for one in selection.diverged.clone() {
                recorder.write(&Event::Trace(TraceMessage::Diverged {
                    turn,
                    path: one.path,
                    turns: one.turns,
                    asking: one.asking,
                    bodies: one.bodies,
                }));
            }
            for one in selection.superseded.clone() {
                recorder.write(&Event::Trace(TraceMessage::Superseded {
                    turn,
                    path: one.path,
                    turns: one.turns,
                    spans: one.spans,
                    tokens: one.tokens,
                    counter: one.counter,
                }));
            }
            // Before the call, not after: this is what we decided to send, and
            // a cancelled turn has it too.
            recorder.write(&Event::Trace(TraceMessage::Budget {
                turn,
                limit: selection.limit,
                counter: selection.counter.clone(),
                buckets: selection.buckets.clone(),
            }));
        }

        let request = CompletionRequest {
            tools: agency.specs(tool_calls),
            model: model.clone(),
            messages: selection.messages,
            // The window we budgeted against, sent so the server serves it.
            // Budgeting 8k against a server truncating to 4k measures a prompt
            // the model never saw, and nothing in the recording would say so.
            context_limit: budget.limit,
            temperature,
            seed,
            constraint: constraint.clone(),
        };

        let (stop, cancel) = watch::channel(false);
        if let Some(ms) = cancel_after_ms {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                let _ = stop.send(true);
            });
        }

        let (tx, mut rx) = mpsc::channel::<TurnEvent>(256);
        let printer = {
            let recorder = recorder.clone();
            let prompt = prompt.clone();
            let prefix = prefix.clone();
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut out = stdout();
                if multi {
                    // A transcript of a scripted run is unreadable without the
                    // questions in it.
                    let _ = out.write_all(format!("\n> {prompt}\n\n").as_bytes()).await;
                }
                while let Some(event) = rx.recv().await {
                    // A model call is not a protocol message: it explains the
                    // agent rather than driving it. Measured into the same
                    // chain as the turns, from the second call on — the first
                    // is the turn's own prompt and is already in it — and a
                    // schema retry counts too, even at `step == 1`: it is a
                    // second call the tracker's own chain has not seen yet,
                    // and skipping it is the gap named in
                    // `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`'s
                    // "the tool-call probe's own instrument cannot see a
                    // retry".
                    if let TurnEvent::ModelCall {
                        step,
                        messages,
                        retry,
                    } = &event
                    {
                        if let (Some(recorder), true) = (recorder.as_ref(), *step > 1 || *retry) {
                            let text = rendered(messages);
                            let mut tracker = prefix
                                .lock()
                                .expect("the prefix tracker is never held across an await");
                            if let Some(TraceMessage::PrefixReuse {
                                shared_bytes,
                                shared_tokens,
                                prompt_tokens,
                                ..
                            }) = tracker.measure(turn, &text, counter.as_ref())
                            {
                                recorder.write(&Event::Trace(TraceMessage::StepCall {
                                    turn,
                                    step: *step,
                                    text,
                                    prompt_tokens,
                                    shared_bytes,
                                    shared_tokens,
                                }));
                            }
                        }
                        continue;
                    }
                    if let Some(recorder) = recorder.as_ref()
                        && let Some(message) = ServerMessage::from_turn_event(turn, event.clone())
                    {
                        recorder.write(&Event::Protocol(message));
                    }
                    match event {
                        // Written and flushed per token on purpose: this is the
                        // CLI's whole job at this stage — showing that
                        // generation streams.
                        TurnEvent::Token(text) => {
                            let _ = out.write_all(text.as_bytes()).await;
                            let _ = out.flush().await;
                        }
                        TurnEvent::ToolCall { step, call } => {
                            let arguments = serde_json::to_string(&call.arguments)
                                .unwrap_or_else(|_| "{}".into());
                            let _ = out
                                .write_all(
                                    format!("\n\n  [{step}] → {} {arguments}\n", call.name)
                                        .as_bytes(),
                                )
                                .await;
                            let _ = out.flush().await;
                        }
                        TurnEvent::ToolResult { step, outcome } => {
                            // The verdict is on the line, always. "The agent
                            // ran a command" and "the kernel held the command
                            // it ran" are different facts.
                            let verdict = &outcome.outcome.verdict;
                            // The rule is printed once. On a denial the error
                            // is the rule with a word in front of it, and
                            // saying it twice reads as two findings.
                            let status = match (verdict.allowed, &outcome.outcome.error) {
                                (false, _) => "denied",
                                (true, Some(error)) => error.as_str(),
                                (true, None) => "ok",
                            };
                            let _ = out
                                .write_all(
                                    format!(
                                        "  [{step}] ← {status} · {} · held by {} · {} ms\n\n",
                                        verdict.rule, verdict.enforced_by, outcome.duration_ms,
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            let _ = out.flush().await;
                        }
                        TurnEvent::Ended { reason, usage } => {
                            let _ = out.write_all(b"\n").await;
                            let counts = match usage {
                                Some(u) => format!(
                                    "{} prompt / {} completion",
                                    u.prompt_tokens, u.completion_tokens
                                ),
                                None => "usage unknown".to_string(),
                            };
                            let reason = match reason {
                                EndReason::Stop => "stop",
                                EndReason::Length => "length",
                                EndReason::Other => "other",
                                EndReason::ToolLimit => "tool limit reached",
                                EndReason::Cancelled => "cancelled",
                            };
                            let _ = out
                                .write_all(format!("\n[{reason}] {counts}\n").as_bytes())
                                .await;
                            let _ = out.flush().await;
                        }
                        TurnEvent::Failed(error) => {
                            let _ = out
                                .write_all(format!("\n\n[failed] {error}\n").as_bytes())
                                .await;
                            let _ = out.flush().await;
                        }
                        // Handled above, before the protocol conversion: it is
                        // not a protocol message and it is not printed.
                        TurnEvent::ModelCall { .. } => {}
                        // `chat` passes no asker, so the loop never holds a
                        // call here.
                        TurnEvent::CallHeld { .. } => {}
                        // Loud, the way a known-incompatible backend's caveat
                        // is at startup — on stderr rather than into the
                        // transcript `out` carries, so a run compared against
                        // one made before this existed is not diffing a note
                        // neither one asked for.
                        TurnEvent::ConstraintRefused { error } => {
                            eprintln!("note: a constrained retry was refused: {error}");
                        }
                    }
                }
            })
        };

        // Inside a task, the plan it was approved with; outside one, the policy
        // file. Never both.
        let sandbox = narrowed.clone().unwrap_or_else(|| agency.sandbox.clone());
        let outcome = run_agent_turn(
            backend.as_ref(),
            request,
            agency.executor(),
            sandbox.as_ref(),
            agency.limits,
            schema_retry.as_ref(),
            // `chat` has no floor and no gate: the policy file is its
            // standing approval, so there is nothing a person could add.
            None,
            tx,
            cancel,
        )
        .await;
        let _ = printer.await;

        // A cancelled turn keeps its partial answer: it happened, and the next
        // turn was asked in its light. A turn that produced nothing is not
        // remembered — an empty assistant message is not a thing that happened.
        if !outcome.text.is_empty() || !outcome.steps.is_empty() {
            context.push_turn_with_steps(
                turn,
                prompt.clone(),
                outcome.text,
                code,
                outcome.steps,
                counter.as_ref(),
            );
        }

        // A script does not push on through a broken backend: the remaining
        // turns would all fail the same way and bury the first, real error.
        if outcome.error.is_some() {
            failed = true;
            break;
        }
    }

    // Let the recorder task drain before the process exits.
    tokio::time::sleep(Duration::from_millis(50)).await;

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// The machine's table, the run's flags, and which wins.
    ///
    /// One-way for `prune` and `results`, both ways for `repeat`, and the
    /// difference is which defaults have moved: a `bool` flag can only turn a
    /// rule on, so for the two that are still off *not given* and *given off*
    /// arrive as the same `false`. `repeat` got `--no-repeat-once` when its
    /// default flipped, because otherwise the flip would have made `always`
    /// unreachable from a command line. The test pins all of it — including the
    /// half that is a limitation rather than a feature — because a precedence
    /// nobody wrote down is one somebody discovers by being surprised.
    /// A posture is described by its `[worker]` block without being built,
    /// and a runtime this machine does not have is named rather than offered.
    #[test]
    fn a_posture_says_where_it_would_run_without_starting_anything() {
        #[derive(clap::Parser)]
        struct Line {
            #[command(flatten)]
            sandbox: SandboxArgs,
        }
        use clap::Parser;
        let dir = std::env::temp_dir().join(format!("luu-describe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory");
        let host = dir.join("host.toml");
        std::fs::write(&host, "[sandbox]\n").expect("a policy file");
        // Apple's runtime: on no Linux runner and not on this Mac, so absent
        // wherever this runs today.
        let apple = dir.join("apple.toml");
        std::fs::write(
            &apple,
            "[worker]\nruntime = \"container\"\nimage = \"luu-worker:dev\"\n",
        )
        .expect("a policy file");

        let args = Line::parse_from(["luu", "--sandbox", &host.display().to_string()]).sandbox;
        let own = args.describe_posture(None, None);
        assert_eq!(own.runtime, "host");
        assert_eq!(own.missing, None, "{own:?}");
        assert!(own.policy.ends_with("host.toml"), "{own:?}");

        let there = args.describe_posture(Some(&apple), None);
        assert_eq!(there.runtime, "container");
        assert_eq!(there.image.as_deref(), Some("luu-worker:dev"));
        if agent_core::worker::which("container").is_none() {
            assert!(
                there
                    .missing
                    .as_deref()
                    .unwrap_or("")
                    .contains("not installed"),
                "{there:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flag_beats_the_file_and_only_repeat_can_be_turned_off_again() {
        let table = |repeat, prune, results| crate::provider::Resend {
            repeat,
            prune,
            results,
        };
        let says_nothing = table(None, None, None);

        // No file and no flags: the defaults as they stand after 2026-09-19 —
        // rule A on because it is the one that has been measured against a
        // model, B and C off because they have not.
        assert_eq!(
            resend_rules(says_nothing, None, false, false),
            (Repeat::Once, Prune::Never, Results::Kept),
        );

        // The file alone decides where no flag was typed.
        assert_eq!(
            resend_rules(
                table(Some(Repeat::Always), Some(Prune::Behind), None),
                None,
                false,
                false,
            ),
            (Repeat::Always, Prune::Behind, Results::Kept),
            "an unnamed key is the code's default, not the file's silence read as off",
        );

        // A flag wins over a file that says otherwise, in both directions for
        // the rule that has a flag in both directions.
        assert_eq!(
            resend_rules(
                table(Some(Repeat::Always), None, None),
                Some(true),
                false,
                false
            )
            .0,
            Repeat::Once,
            "a flag typed at the run is more specific than a machine's default",
        );
        assert_eq!(
            resend_rules(
                table(Some(Repeat::Once), None, None),
                Some(false),
                false,
                false
            )
            .0,
            Repeat::Always,
            "--no-repeat-once is the way back, and a file saying `once` does not outrank it",
        );
        assert_eq!(
            resend_rules(says_nothing, Some(false), false, false).0,
            Repeat::Always,
            "and it reaches past the code's own default, which is the case it exists for",
        );

        // And the half that is still a limitation: B and C have no flag that
        // says off, so a file that turns one of them on is the end of it.
        assert_eq!(
            resend_rules(table(None, Some(Prune::Behind), None), None, false, false).1,
            Prune::Behind,
            "nothing on the command line spells off for this one, so this is what it does",
        );
    }

    /// The two flags as one answer, including the arm clap makes unreachable.
    #[test]
    fn neither_flag_is_the_only_way_to_ask_the_file() {
        assert_eq!(repeat_asked(false, false), None, "the file gets asked");
        assert_eq!(repeat_asked(true, false), Some(true));
        assert_eq!(repeat_asked(false, true), Some(false));
        assert_eq!(
            repeat_asked(true, true),
            Some(false),
            "`conflicts_with` makes this a parse error; if it ever stops being \
             one, the flag that says off is the one a person typed on purpose",
        );
    }

    /// The default itself, pinned so that moving it again is a diff somebody
    /// reads rather than a number that drifts.
    ///
    /// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`
    /// §What it costs asks for exactly this test, and it asks for it because
    /// the decision it pins is **n=1**: one conversation, one 7B, one
    /// quantization, one window, one seed, run once. A second corpus that
    /// contradicts the 2026-09-14 run flips this back, and that is a line in
    /// this file rather than a rediscovery.
    #[test]
    fn a_budget_that_is_asked_nothing_renders_each_span_once() {
        let budget = Budget::new(8192, 512, agent_core::context::Eviction::Turn);
        assert_eq!(budget.repeat, Repeat::Once);
        assert_eq!(budget.prune, Prune::Never, "unmeasured, so off");
        assert_eq!(budget.results, Results::Kept, "unmeasured, so off");
    }

    #[test]
    fn a_script_without_tasks_parses_the_way_it_always_did() {
        let steps = parse_script("# a comment\n\nfirst question\nsecond question\n").unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Prompt("first question".into()),
                Step::Prompt("second question".into()),
            ]
        );
    }

    #[test]
    fn a_task_carries_its_approved_plan() {
        let steps = parse_script(
            "## task: explain the context manager\n\
             ## step: read the design\n\
             ## file: luu-design.md\n\
             ## command: cargo\n\
             what does it do?\n\
             ## close\n",
        )
        .unwrap();

        assert_eq!(steps.len(), 3);
        let Step::OpenJob { objective, plan } = &steps[0] else {
            panic!("{steps:?}")
        };
        assert_eq!(objective, "explain the context manager");
        assert_eq!(plan.tasks, ["read the design"]);
        assert_eq!(plan.files, ["luu-design.md"]);
        assert_eq!(plan.commands, ["cargo"]);
        assert_eq!(steps[2], Step::CloseJob);
    }

    #[test]
    fn a_job_carries_its_approved_egress() {
        let steps = parse_script(
            "## job: fetch updates\n\
             ## egress: crates.io, *.github.com\n\
             fetch them\n\
             ## close\n",
        )
        .unwrap();

        assert_eq!(steps.len(), 3);
        let Step::OpenJob { plan, .. } = &steps[0] else {
            panic!("{steps:?}")
        };
        assert!(plan.network);
        assert_eq!(plan.egress, ["crates.io", "*.github.com"]);
    }

    #[test]
    fn a_fragment_attaches_to_the_next_prompt_and_is_not_part_of_a_plan() {
        let steps =
            parse_script("## fragment: luu.toml:1-5\n## task: a\n## file: luu.toml\nq\n## close\n")
                .unwrap();
        assert_eq!(
            steps[0],
            Step::Fragment("luu.toml:1-5".into()),
            "it stands on its own: grounding for a turn, not a path the plan declares",
        );
        let Step::OpenJob { plan, .. } = &steps[1] else {
            panic!("expected the job after it, {steps:?}");
        };
        assert_eq!(plan.files, ["luu.toml"]);

        assert!(
            parse_script("## fragment:\nq\n")
                .unwrap_err()
                .to_string()
                .contains("needs a path")
        );
    }

    /// Every path the scripts in `scripts/tasks/` name, checked against the
    /// tree they name it in.
    ///
    /// `crates/agent-core/src/task.rs` was renamed to `job.rs` in `d70e6d9` and
    /// both grounded scripts kept pointing at it — five of twenty prompts in
    /// each, an error rather than a warning, for four days. Nothing said so
    /// because nothing ran them, and the same failure mode (a corpus that rots
    /// while the tree moves under it) had already produced a stale coverage
    /// table once. The second occurrence is what makes it a test: this runs on
    /// every commit, so a rename breaks the build rather than the next run.
    ///
    /// It parses with `parse_script`, not with a second reader — a guard that
    /// reads the directives differently from the program guards a different
    /// file.
    ///
    /// **A script is checked against the tree it runs against, which is not
    /// always this one.** `edit-reread.txt` edits the files it reads, so it
    /// runs against a copy of `scripts/tasks/edit-reread/` and its paths are
    /// relative to that; a directory beside a script and carrying its name is
    /// that script's tree, and everything else resolves against the repository
    /// root as it always has. The rule is a convention rather than a directive
    /// because it changes only where a path is *looked up*: the script itself
    /// is parsed by `parse_script` like every other, and inventing a `##
    /// project:` the runner would have to ignore would put a line in a corpus
    /// that means nothing at run time.
    #[test]
    fn every_script_names_a_file_that_exists() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dir = root.join("scripts/tasks");
        let mut checked = 0;
        let mut rooted_elsewhere = 0;

        let mut scripts: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("scripts/tasks")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
            .collect();
        scripts.sort();
        assert!(!scripts.is_empty(), "no scripts found in {}", dir.display());

        for script in &scripts {
            let name = script.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(script).expect(&name);
            let steps = parse_script(&text).unwrap_or_else(|e| panic!("{name}: {e}"));

            // The tree this script's paths are relative to: the directory
            // beside it carrying its own name, or the repository.
            let beside = script.with_extension("");
            let root = if beside.is_dir() {
                rooted_elsewhere += 1;
                beside
            } else {
                root.clone()
            };

            // A `## fragment:` carries a range, and a range past the end is an
            // error at run time — so it is one here too, where it is cheap.
            let fragments = steps.iter().filter_map(|step| match step {
                Step::Fragment(spec) => Some(spec),
                _ => None,
            });
            for spec in fragments {
                let spec = fragment::Spec::parse(spec);
                let path = root.join(&spec.path);
                let body = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{name}: `## fragment: {}` — {e}", spec.source));
                if let Some((start, end)) = spec.lines {
                    let had = body.lines().count();
                    assert!(
                        start <= had,
                        "{name}: `## fragment: {}` starts past the end of a {had}-line file",
                        spec.source,
                    );
                    assert!(
                        end <= had,
                        "{name}: `## fragment: {}` ends past the end of a {had}-line file",
                        spec.source,
                    );
                }
                checked += 1;
            }

            // `## file:` and `## write:` are the plan's, not the prompt's, and
            // a plan naming a path the tree has lost is approved against
            // nothing.
            let plans = steps.iter().filter_map(|step| match step {
                Step::OpenJob { plan, .. } => Some(plan),
                _ => None,
            });
            for plan in plans {
                for declared in plan.files.iter().chain(plan.writes.iter()) {
                    assert!(
                        root.join(declared).exists(),
                        "{name}: the plan declares `{declared}`, which is not in the tree",
                    );
                    checked += 1;
                }
            }
        }

        assert!(
            checked > 0,
            "the scripts named no paths at all — did the directives change?"
        );
        // Without this the convention could be deleted — the directory renamed,
        // the corpus left pointing at it — and every remaining path would
        // resolve against the repository root, where `src/greeting.rs` does not
        // exist, so the failure would at least be loud. What it could *not*
        // catch is the other direction: a script whose tree quietly stops being
        // consulted because nothing here says one ever was.
        assert!(
            rooted_elsewhere > 0,
            "no script has a tree of its own beside it — \
             did `scripts/tasks/edit-reread/` move?"
        );
    }

    #[test]
    fn a_mistyped_directive_is_refused_rather_than_asked_as_a_question() {
        // The one directive whose value is a strictness rather than a grant:
        // the parser takes both spellings and refuses a third.
        let steps = parse_script("## job: build\n## enforcement: kernel\nq\n").unwrap();
        let Step::OpenJob { plan, .. } = &steps[0] else {
            panic!("a job opens the script");
        };
        assert_eq!(
            plan.enforcement,
            Some(agent_core::sandbox::Enforcement::Kernel),
        );
        assert!(
            parse_script("## job: b\n## enforcement: sort-of\nq\n")
                .unwrap_err()
                .to_string()
                .contains("expects kernel or best-effort"),
        );

        let error = parse_script("## task: x\n## fille: y\nq\n").unwrap_err();
        assert!(
            error.to_string().contains("is not a directive"),
            "a typo that becomes a prompt would sit in a baseline unnoticed: {error}",
        );
    }

    #[test]
    fn the_lifecycle_has_to_make_sense_in_the_file() {
        assert!(
            parse_script("## close\nq\n")
                .unwrap_err()
                .to_string()
                .contains("no job open")
        );
        assert!(
            parse_script("## task: a\nq\n## task: b\nq\n")
                .unwrap_err()
                .to_string()
                .contains("still open")
        );
        assert!(
            parse_script("## step: read it\nq\n")
                .unwrap_err()
                .to_string()
                .contains("must follow a `## job:`")
        );
        assert!(
            parse_script("## task: a\nq\n## step: late\n")
                .unwrap_err()
                .to_string()
                .contains("before its first prompt"),
            "a plan cannot grow after a turn it was supposed to authorise",
        );
    }

    #[test]
    fn a_script_of_directives_and_no_questions_is_not_a_script() {
        assert!(
            parse_script("## task: a\n## close\n")
                .unwrap_err()
                .to_string()
                .contains("no prompts")
        );
    }
}
