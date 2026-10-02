//! What earns its place in the prompt.
//!
//! Two commitments shape everything here, and both look like implementation
//! details from close up:
//!
//! - **Decide, then render.** The selection happens against a token budget and
//!   the rendering is a pure function of it, so every token sent can be
//!   attributed to a bucket. Rendering first and trimming the string afterwards
//!   loses that, and cuts wherever the limit happens to land.
//! - **The stable prefix stays byte-identical.** The system block is the part
//!   the prompt cache reuses. Nothing that changes per turn goes above it —
//!   selected code is fused into the *current* user message, as late as
//!   possible.
//!
//! See `RECORD/2026-08-27.context-manager.completed.md` for how both were arrived at.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::backend::Message;
use crate::job::{ApprovedBy, ClosedBy, Job, JobId, Plan, Replaced, TaskId};
use crate::protocol::TurnId;
use crate::tools::ToolStep;
use crate::trace::Bucket;

/// Which counter produced a token count.
///
/// Carried with every stored count and every budget, because two runs measured
/// by different counters are not comparable — and nothing else would say so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Counter {
    /// The model's own tokenizer, named by whatever identifies it to a reader.
    Model { id: String },
    /// A stand-in for when the tokenizer cannot be loaded. It exists so a
    /// missing file degrades instead of failing, and it is labelled everywhere
    /// it appears: it is not a measurement and must never be read as one.
    Approximate,
}

impl Counter {
    pub fn is_approximate(&self) -> bool {
        matches!(self, Self::Approximate)
    }
}

/// Counts tokens the way the loaded model would.
pub trait TokenCounter: Send + Sync {
    fn id(&self) -> Counter;
    fn count(&self, text: &str) -> u32;
}

/// Characters over four. Wrong by enough to matter, and useful only so that a
/// missing `tokenizer.json` is an annotation rather than a dead end.
pub struct ApproximateCounter;

impl TokenCounter for ApproximateCounter {
    fn id(&self) -> Counter {
        Counter::Approximate
    }

    fn count(&self, text: &str) -> u32 {
        text.chars().count().div_ceil(4) as u32
    }
}

#[derive(Debug, thiserror::Error)]
#[error("loading the tokenizer at {path}: {message}")]
pub struct TokenizerError {
    pub path: String,
    pub message: String,
}

/// The model's own tokenizer, from its `tokenizer.json`.
pub struct ModelCounter {
    tokenizer: tokenizers::Tokenizer,
    id: String,
}

impl ModelCounter {
    /// `id` is what a reader will see in a recording months later, so it should
    /// name the model rather than the file.
    pub fn from_file(
        path: &std::path::Path,
        id: impl Into<String>,
    ) -> Result<Self, TokenizerError> {
        let tokenizer = tokenizers::Tokenizer::from_file(path).map_err(|error| TokenizerError {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        Ok(Self {
            tokenizer,
            id: id.into(),
        })
    }
}

impl TokenCounter for ModelCounter {
    fn id(&self) -> Counter {
        Counter::Model {
            id: self.id.clone(),
        }
    }

    fn count(&self, text: &str) -> u32 {
        // Special tokens are the chat template's business, and the template is
        // applied by the backend where we cannot see it. That gap is accepted
        // and reported rather than guessed at — see the record.
        match self.tokenizer.encode(text, false) {
            Ok(encoding) => encoding.len() as u32,
            // A tokenizer that loaded but cannot encode is not a case worth a
            // Result on every call site; the count degrades and the trace
            // still says which counter was in use.
            Err(_) => ApproximateCounter.count(text),
        }
    }
}

/// A piece of code selected for one turn.
///
/// `path` is the spec as it was written — `src/lib.rs:12-40`, not a canonical
/// path — so it carries the span, and two fragments are the same fragment when
/// the path *and* the bytes match. The bytes are half of that on purpose: a
/// file edited between two turns yields the same spec and different text, and
/// `resolving-a-symbol` is the record of what happens when the second one is
/// mistaken for the first.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fragment {
    pub path: String,
    pub text: String,
}

/// A context rebuilt from a stored view, and what could not be rebuilt with it.
///
/// Two fields rather than a bare `Context`, for the reason `Selection` carries
/// `evicted` and `pruned` rather than logging them: the thing that knows a span
/// could not be read is not the thing that knows who to tell. A resume that
/// restored four spans of six and said nothing would be the 2026-09-17 finding
/// one surface along — the refusal erased by the thing it describes.
#[derive(Debug)]
pub struct Restored {
    pub context: Context,
    /// One entry per span that was in the view and is not in the window,
    /// oldest turn first. Empty is the ordinary case and means every span came
    /// back.
    pub unreadable: Vec<Unreadable>,
}

/// A span a resume could not read again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub turn: TurnId,
    /// The spec, as the turn that was grounded named it.
    pub spec: String,
    /// The load error, whole: a denial that does not name the rule it broke is
    /// unreadable the moment a symlink is involved, and this is the only place
    /// the reason survives.
    pub why: String,
}

/// One exchange. The unit of everything the context manager does: eviction
/// drops a turn, compaction replaces one, relevance scores one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// The turn number the session handed out, so that a turn dropped from the
    /// window can be *named* rather than counted.
    ///
    /// Assigned by the caller rather than by the position in `turns`, because
    /// the two come apart: a turn that produced nothing is never pushed, and
    /// from then on the nth turn of a session is not the nth element here. An
    /// index would be wrong silently, in the one file whose whole purpose is
    /// to be trusted months later.
    pub id: TurnId,
    pub prompt: String,
    pub answer: String,
    /// What the agent did before answering. Part of the turn, not of the
    /// history around it: a turn with tool calls is evicted whole, so the model
    /// never sees a result whose call has gone — and until the next turn is
    /// evicted it can see that it already read the file.
    #[serde(default)]
    pub steps: Vec<ToolStep>,
    /// Rendered fused into the user message, stored apart so that pruning can
    /// reach it later without parsing back what we already wrote.
    pub code_context: Vec<Fragment>,
    /// The job this turn was asked inside.
    ///
    /// Total, and that is the whole of item 22 in one field: every turn belongs
    /// to a job from the session's first turn, because a turn landing where
    /// nothing is open **opens a draft** to land in. What used to be `None` was
    /// the turns before an approval, and the opening draft holds them now.
    ///
    /// It stays `Option` on the wire — see [`crate::api::TurnView::job`] —
    /// because *absent* there means *written before this existed*, which is a
    /// different claim from *this turn was in a draft*, and inventing one for
    /// the other is the conversion's business rather than this type's.
    #[serde(alias = "task")]
    pub job: JobId,
    /// Counted once, when the turn closed. A closed turn does not change, and
    /// re-counting every turn on every turn is quadratic over a session.
    pub tokens: u32,
    /// Which counter produced `tokens`. Without it, swapping tokenizers
    /// mid-session sums two different units into one bar.
    pub counted_by: Counter,
}

impl Turn {
    /// Backwards compatibility accessor for callers that called `turn.task`.
    pub fn task(&self) -> JobId {
        self.job
    }
}

/// How the history gives way when the next turn no longer fits.
///
/// Not a preference: the two rewrite the prompt at completely different rates,
/// and a prefix cache reuses the longest common prefix. See
/// `RECORD/2026-08-27.prefix-reuse-and-block-eviction.completed.md`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum Eviction {
    /// Drop exactly as many turns as it takes to fit. The baseline, and the
    /// thing to beat: once the window is full it drops one turn per turn, so
    /// the history block is rewritten from its front on every call and the
    /// reusable prefix collapses to the system block.
    Turn,
    /// When it no longer fits, drop past a low-water mark — a fraction of the
    /// history budget — instead of dropping the minimum. The cut is deeper and
    /// far less frequent: the history is rewritten once every N turns and stays
    /// byte-identical in between, which is what a prefix cache pays for.
    ///
    /// `low_water` is ours and invented, not inherited from anyone's tuned
    /// workload. It is a flag so that it can be measured.
    Block { low_water: f32 },
}

/// What a span already in the window costs the turn that selects it again.
///
/// Rule A of `RECORD/2026-09-06.what-leaves-the-history.completed.md`, and the
/// smaller half of it: the `history` bucket of a selecting run is ~90% quoted
/// code by turn 20, and 9.2% of the code tokens in the precision corpus are a
/// span some earlier turn already put in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    /// Send it again, in every turn that selected it. Everything recorded
    /// before this enum existed, and what this was the default for as long as
    /// the rule was unmeasured, because a run made under a rule that did not
    /// exist is not comparable to one made under it.
    ///
    /// No longer the default, and that is a decision with a date on it rather
    /// than a preference: `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`
    /// §fourteenth. Still reachable — from `[resend]`, from the page, and from
    /// `--no-repeat-once` — because a recording made under it stays readable
    /// only if the arm that made it can still be asked for.
    Always,
    /// Render it once, in the **oldest** turn of the window that carries it.
    ///
    /// Oldest and not newest, and that is the whole decision. The alternative
    /// rewrites a turn that is already in the prefix every time a later turn
    /// repeats one of its spans, which is rule B's cost and rule B's
    /// measurement; this way only the newest message changes, which is the one
    /// that changes anyway.
    ///
    /// The owner is chosen inside the window being rendered, never over the
    /// whole session, so eviction cannot orphan a span: dropping the turn that
    /// owned one hands it to the next turn that carries it, in the same call.
    /// Choosing an owner that may already be below the floor is
    /// `the-fold-fix-verified`'s failure with a different mechanism, and the
    /// precision corpus offers it 22 chances.
    ///
    /// **The default since 2026-09-19**, and the only one of the three rules
    /// that has been through a model: 20 grounded questions on
    /// `qwen2.5-coder:7b`, verdict for verdict identical to `always`, with the
    /// `code` bucket down 17.5%. The criterion it wins under is *fewest tokens
    /// without affecting the result*, not *fewest tokens* — that one selects
    /// rule C, which has no accuracy measurement at all. It is n=1 and the
    /// record says so where it decides it.
    Once,
}

/// What an older turn's quoted code costs once the window stops fitting.
///
/// Rule B of `RECORD/2026-09-06.what-leaves-the-history.completed.md`, argued in
/// `RECORD/2026-09-08.prune-behind.completed.md`, and the larger half: rule A reaches
/// the spans a *later* turn selected again, and this reaches the ones nobody
/// did — which by turn 20 of the precision run is most of a history block that
/// is ~90% quoted code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Prune {
    /// An old turn carries its spans in full until it is evicted whole.
    /// Everything recorded before this enum existed, and the default.
    Never,
    /// Under pressure, the oldest turns of the window give up their spans and
    /// keep their exchange: the code becomes the line that cites it, the
    /// question and the answer stay.
    ///
    /// A ratchet, like [`Context::floor`] and for the same reason — a decision
    /// the window took has to outlive the call that took it, or the prompt
    /// oscillates and no prefix survives two calls. It moves to the target the
    /// eviction policy has already computed, so depth against frequency stays
    /// one trade with one knob, and the floor only moves when pruning the whole
    /// window is not enough.
    Behind,
}

/// What an old turn's *tool output* costs once its turn is behind the prune
/// line.
///
/// The third rule over the same window, argued in
/// `RECORD/2026-09-09.what-a-result-costs.completed.md`, and deliberately not a new
/// idea: an 8 KiB `cat` is ~2 000 tokens at the approximate counter — twice
/// what a whole turn selects at `--select-tokens 1024` — charged to every turn
/// after the one that made the call, and until now the only thing that ever
/// took it out of the prompt was evicting that turn with its question and its
/// answer.
///
/// It has no line of its own. [`Prune::Behind`] already decides which turns are
/// old enough to give something up; this decides whether tool output is one of
/// the things they give up, which is why it is inert on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Results {
    /// A tool result is sent in full for as long as its turn is in the window.
    /// Everything recorded before this enum existed, and the default.
    Kept,
    /// Behind the prune line, a result that **could be asked for again**
    /// becomes the line that cites it, and one that is a record of something
    /// that happened stays.
    ///
    /// The cut is `could_be_read_again`, and the argument for it is that the
    /// number rule C is justified by was never measured on the half this
    /// keeps: every one of the 20 turns that freed 32 752 tokens called
    /// `read_file` on a 136 KB file and hit the 8 KiB cap, and **no corpus in
    /// this repository has ever pruned a command's output.** So this value
    /// ships the whole measured saving while making none of the unmeasured
    /// claim — which is what item 5 of `ROADMAP/2026-09-17` is open about, and
    /// why it is a third value rather than a redefinition of the one below.
    /// See `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`
    /// §third.
    CitedReads,
    /// Behind the prune line the output becomes the line that cites it, and the
    /// call that asked for it stays verbatim.
    ///
    /// The assistant half is never rewritten — it is the model's own words, and
    /// it is why [`crate::tools::ToolStep::text`] is stored rather than
    /// re-rendered from the call. What is replaced is the result *whole*, head
    /// included, so the number in the citation is the cost of exactly what is
    /// no longer there.
    ///
    /// **Every result, a command's included.** Unchanged when `CitedReads`
    /// landed beside it, deliberately: there is a run on disk under this arm,
    /// and a value that quietly starts meaning something narrower turns a
    /// recorded arm into an unrecorded one — the sentence rule C's own flag
    /// exists because of.
    Cited,
}

/// Whether a tool's output could be had again by asking the same question.
///
/// The cut [`Results::CitedReads`] makes, and the whole of it. A file's bytes
/// and a directory listing are a *view* — the thing they describe is still
/// there, and the model can look again. A command's output is a **record of
/// something that happened**: an exit code, a test run, the thing a closing
/// condition reads. Replacing that with a line saying it cost 1 987 tokens
/// tells the model the call was made and nothing about what it found.
///
/// **Unknown names are evidence.** A tool this function has not heard of keeps
/// its output, because what it produces is exactly what nobody has decided yet
/// — a sixth tool arriving as *cheap to lose* would be the failure rule C's
/// own flag was split off to avoid, one level down. `write_file` and
/// `edit_file` are in that group too and cost nothing to leave there: their
/// results are confirmations, and `pruned_result_text`'s guard already refuses
/// to cite anything a citation would not shrink.
pub fn could_be_read_again(tool: &str) -> bool {
    matches!(tool, "read_file" | "list_dir")
}

/// The window, what is held back from it, and how it gives way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// The model's context window. `None` means unknown — then there is no
    /// budget to spend, so nothing is selected and nothing is evicted.
    pub limit: Option<u32>,
    /// Room for the answer, set aside before any history is considered.
    pub reserve: u32,
    pub eviction: Eviction,
    /// What a span already visible in the window costs to select again.
    pub repeat: Repeat,
    /// What an old turn's spans cost once the window no longer fits.
    pub prune: Prune,
    /// What an old turn's tool output costs once its turn is behind the prune
    /// line. Inert without [`Prune::Behind`]: nothing is behind a line that
    /// never moves.
    pub results: Results,
}

impl Budget {
    /// The CLI spells "unknown" as 0, because a flag has to have a default.
    ///
    /// `repeat` is [`Repeat::Once`] and the other two are off, which is not
    /// three rules ranked by what they save — ranked that way the winner would
    /// be `results`, the one of the three that has never been put in front of a
    /// model. `repeat` is here because it is the only one whose saving has been
    /// shown to cost nothing, on n=1, which is stated where it is decided.
    pub fn new(limit: u32, reserve: u32, eviction: Eviction) -> Self {
        Self {
            limit: (limit > 0).then_some(limit),
            reserve,
            eviction,
            repeat: Repeat::Once,
            prune: Prune::Never,
            results: Results::Kept,
        }
    }

    /// The rule this budget renders spans under. **On** by default since
    /// 2026-09-19, so this is as much the way back to [`Repeat::Always`] as the
    /// way to [`Repeat::Once`] — every recording on disk was made under one of
    /// the two, and the header says which since format 11.
    pub fn repeating(self, repeat: Repeat) -> Self {
        Self { repeat, ..self }
    }

    /// Off by default, for the reason `repeating` is: this one rewrites the
    /// history block where it fires, so a recording made under it is not the
    /// same arm as one made without it.
    pub fn pruning(self, prune: Prune) -> Self {
        Self { prune, ..self }
    }

    /// Off by default, and its own flag rather than a widening of `pruning`:
    /// there is a run on disk made under `--prune-behind`, and a flag that
    /// quietly starts meaning something else turns a recorded arm into an
    /// unrecorded one.
    pub fn citing(self, results: Results) -> Self {
        Self { results, ..self }
    }
}

/// What was decided, and the rendering of it. The two are produced together on
/// purpose: `buckets` describes `messages`, not an estimate of it.
#[derive(Debug, Clone)]
pub struct Selection {
    pub messages: Vec<Message>,
    /// In prompt order, with the reserve last — a stacked bar reads as the
    /// prompt reads.
    pub buckets: Vec<Bucket>,
    pub limit: Option<u32>,
    pub counter: Counter,
    /// How many whole turns the window has lost since the session started —
    /// the floor, as a number.
    pub evicted: usize,
    /// How many turns of the window have given up their spans, as an index into
    /// the session's turns — the prune line, and a ratchet like the floor.
    pub pruned: usize,
    /// What *this* selection dropped, when it dropped anything. `None` is a
    /// selection that cut nothing, which is every one in a session that never
    /// fills its window.
    pub eviction: Option<Evicted>,
    /// What this selection pruned, if the line moved. `None` on a selection
    /// that pruned nothing, which is every one made under [`Prune::Never`].
    pub pruning: Option<Pruned>,
    /// What rule A kept out of this render. `None` when it kept nothing out,
    /// which is every selection made under [`Repeat::Always`] and every one
    /// under [`Repeat::Once`] whose window carries no span twice.
    pub repeating: Option<Repeated>,
    /// Paths this render sent under more than one body, in the order the window
    /// first carried them.
    ///
    /// A `Vec` rather than the `Option<_>` the two fields above use, because
    /// there is no single event here to be absent: each path is its own finding
    /// and an empty vector is the ordinary case. Every other selection in this
    /// session produces one too — it is the window that is being described, not
    /// a thing that happened to it.
    pub diverged: Vec<Diverged>,
    /// Paths this render sent a stale body for as a citation rather than as
    /// bytes, in the order the window first carried them.
    ///
    /// Empty under [`Repeat::Always`], always and by construction: there a
    /// prompt carrying one path twice is the documented behaviour — every turn
    /// re-sends what it selected and the blocks read as per-turn snapshots — so
    /// a run made without rule A cannot be quietly changed by rule A's repair.
    pub superseded: Vec<Superseded>,
}

/// What one selection dropped from the window — and it stays dropped: the
/// floor only ever moves forward.
///
/// Carried out of [`Context::select`] rather than logged inside it, because the
/// context manager does not know which turn is about to be sent. The caller
/// does, and it is the one that puts this on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evicted {
    /// The turns that left, oldest first. Named rather than counted: a reader
    /// months later cannot recover *which* without re-implementing the floor,
    /// and a transcript cannot mark them.
    pub turns: Vec<TurnId>,
    /// What they were worth in the prompt they are no longer in.
    pub tokens: u32,
    /// Which counter produced `tokens`. Every count in this system says who
    /// produced it, because two runs measured differently are not comparable
    /// and nothing else would say so.
    pub counter: Counter,
    /// Which cut this was. The whole difference between the policies is how
    /// deep they go, so a tombstone that did not name the policy would leave
    /// the reader doing the subtraction it exists to spare them.
    pub policy: Eviction,
}

/// What one selection took out of the prompt without taking the turn with it.
///
/// Beside [`Evicted`] and deliberately not the same thing: an evicted turn is
/// gone from the conversation, and a pruned one is still asked, still answered
/// and still in the transcript — what left is its code. That is why this goes
/// on the trace channel where the buckets are, and eviction goes on the
/// protocol: a client that cannot read this is not wrong about which turns the
/// session has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pruned {
    /// The turns that gave up spans, oldest first. Only the ones that had any:
    /// the line passes over turns with nothing to give, and naming them would
    /// report a saving that did not happen.
    pub turns: Vec<TurnId>,
    /// What the window saved by it — the difference of two windows, not the sum
    /// of the spans, because under [`Repeat::Once`] a span the pruned turn
    /// owned passes to a younger turn instead of leaving.
    pub tokens: u32,
    /// Which counter produced `tokens`.
    pub counter: Counter,
}

/// What rule A kept out of one render: the spans a turn owns and did not send,
/// because something else in this same prompt is already showing them.
///
/// Beside [`Pruned`] and shaped like it, because it is the same kind of fact —
/// a turn gave up code and stayed in the conversation. What makes it a separate
/// report is which rule did it and what the number means, below.
///
/// It exists because rule A is the **default** and was the only one of the
/// three window rules that reported nothing: `split_shown` computed this and
/// the render subtracted it from a bucket, so a run that collapsed forty spans
/// and one that collapsed none left the same evidence. That is, one field
/// along, what `record::FORMAT` 11 was bumped to fix for `--prune-behind`. See
/// `RECORD/2026-09-19.one-counting-surface.completed.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repeated {
    /// The turns of the history that gave up spans, oldest first. Only the ones
    /// that gave up any, for [`Pruned::turns`]' reason: naming a turn that lost
    /// nothing reports a saving that did not happen.
    pub turns: Vec<TurnId>,
    /// Whether the turn being asked gave one up too — the case where the
    /// history is showing a span and the *fresh* read of it is the one that is
    /// dropped.
    ///
    /// A `bool` and not an id, for [`Diverged::asking`]'s reason: this runs
    /// before the turn is pushed and it has no id yet.
    pub asking: bool,
    /// How many fragments went unsent across the whole render.
    pub spans: usize,
    /// What those fragments would have cost, summed, by the counter below.
    ///
    /// **The sum of the spans, and deliberately not what [`Pruned::tokens`]
    /// is.** That field is the difference of two windows precisely because
    /// under [`Repeat::Once`] a span a pruned turn owned passes to a younger
    /// turn instead of leaving; this is the other side of that same passing
    /// down, so it is what the render did not resend and **not** what the
    /// window would have been under [`Repeat::Always`] — where the younger turn
    /// would not have been holding the span in the first place. A reader who
    /// wants the counterfactual runs the arm, which is what one flag apart is
    /// for.
    pub tokens: u32,
    /// Which counter produced `tokens`.
    pub counter: Counter,
}

/// One path the rendered prompt carried with more than one body.
///
/// The defect of `RECORD/2026-09-19.one-path-two-bodies.completed.md`: `code_context`
/// is written once at `push_turn_with_steps` and never refreshed, while every
/// turn re-reads its own spans through the sandbox. So a file edited between two
/// turns is sent twice under one `// path` header, with different contents and
/// nothing saying which is true.
///
/// A report and not a repair. Nothing in the render behaves differently because
/// this is populated — the fix is argued in that record and deliberately not
/// taken, because it trades measured prefix reuse for truthfulness and the
/// frequency it turns on has never been measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diverged {
    /// The span, as the fragment names it — the whole spec, line range
    /// included. Two *overlapping* ranges of one file are two paths here and
    /// their disagreement is not caught; see that record's §Still open.
    pub path: String,
    /// The turns of the *history* that carried it, oldest first, one entry per
    /// turn and not per body: a reader wants to know where to look, and the
    /// bodies themselves are in the prompt this selection just built.
    pub turns: Vec<TurnId>,
    /// Whether the turn being asked is one of the carriers.
    ///
    /// A `bool` beside the ids rather than an id among them, because
    /// [`Context::select`] runs *before* the turn is pushed and it has no id
    /// yet — inventing the next one would put a number in a report that the
    /// caller had not handed out and may not hand out, since a turn that
    /// produces nothing is never pushed.
    ///
    /// It is also the carrier that matters most, and the reason this is
    /// reported at all rather than treated as a curiosity about old turns: the
    /// turn being asked holds the bytes that were read *this* turn, so a
    /// divergence including it is the history contradicting what is on disk
    /// right now.
    pub asking: bool,
    /// How many *distinct* bodies went out under this path. Always at least
    /// two, because one is not a divergence and is never recorded.
    pub bodies: usize,
}

/// One path whose stale bodies this render replaced with the line that cites
/// them, because a later turn read the same span and got different bytes.
///
/// The repair [`Diverged`] reports the need for, and the two are read together:
/// under [`Repeat::Once`] a session that supersedes is a session that no longer
/// diverges, so `diverged` empty beside this non-empty is the fix working
/// rather than nothing happening. See
/// `RECORD/2026-09-20.the-newest-body-wins.completed.md`.
///
/// There is no `asking` field beside [`Diverged::asking`], and its absence is
/// the invariant: the turn being asked holds the newest read of every span it
/// carries, so it is never the turn that is superseded. Only the history is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Superseded {
    /// The span, as the fragment names it — the whole spec, line range
    /// included, exactly as [`Diverged::path`] carries it.
    pub path: String,
    /// The turns of the history whose body was replaced, oldest first. One
    /// entry per turn however many of its spans moved, because a reader wants
    /// to know where to look.
    pub turns: Vec<TurnId>,
    /// How many fragments were replaced across the render.
    pub spans: usize,
    /// What their **bytes** would have cost, summed — the sum of the spans, as
    /// [`Repeated::tokens`] is and for its reason. It is not the saving: a
    /// citation stands where each one stood and costs something of its own,
    /// which the `history` bucket carries because that bucket describes the
    /// messages rather than estimating them.
    pub tokens: u32,
    /// Which counter produced `tokens`.
    pub counter: Counter,
}

/// One unit of rendered history: a live turn, or a closed job folded to its
/// summary. Eviction works over these rather than over turns, so the two ways
/// history gives way — forgetting the oldest and folding the finished —
/// compose instead of cutting each other in half.
#[derive(Debug, Clone, Copy)]
enum Item {
    /// An index into `turns`.
    Turn(usize),
    Folded {
        job: JobId,
        /// Index of the job's first turn still above the floor.
        first: usize,
    },
}

impl Item {
    /// The index of the oldest turn this item covers.
    fn first(&self) -> usize {
        match self {
            Self::Turn(index) => *index,
            Self::Folded { first, .. } => *first,
        }
    }
}

/// The conversation, and the rule for turning it into a prompt.
#[derive(Debug, Clone)]
pub struct Context {
    system: String,
    /// The tool definitions, rendered once. Second half of the cached prefix
    /// and counted as its own bucket, because "the system block grew" is not an
    /// answer to why the window is full.
    tools: String,
    /// Whether `tools` travels in the request's own `tools` field rather than
    /// in the system message — the native transport. Then it is counted here,
    /// as the specs' JSON, because that is what is sent, and left out of
    /// [`Self::system_message`], because the server's template renders it.
    /// See `RECORD/2026-09-30.native-tool-calls.completed.md`.
    tools_native: bool,
    /// The repository outline, rendered once. Last of the cached prefix, which
    /// is where blocks are ordered by how often they are *rewritten*: the
    /// system block is a constant, the tools change when the tool set does, and
    /// the map changes when the repository does. Empty unless asked for.
    map: String,
    turns: Vec<Turn>,
    /// The oldest turn still in the window. It only ever moves forward.
    floor: usize,
    /// The prune line: turns below it render their spans as citations. Moves
    /// forward only, and only under [`Prune::Behind`]. Never below the floor,
    /// because a turn nobody renders has nothing to give up.
    pruned: usize,
    /// What the line took, the last time the window was rendered — the other
    /// half of the state above. The line says *which* turns gave something up
    /// and this says *what*, and a fold reads both: what it stopped sending is
    /// what the prompt it left was actually made of, which under
    /// [`Results::Cited`] is not what those turns are stored at.
    ///
    /// Set on every selection rather than only on the ones that prune, because
    /// it describes the render and not the decision.
    cited: Results,
    /// Rule A as the last render ran it, kept here for [`Self::cited`]'s reason
    /// and no other: a fold has to cost its turns the way the prompt it is
    /// leaving actually carried them, and under [`Repeat::Once`] that means
    /// knowing whether a stale span went out as bytes or as the line that cites
    /// it. Also set on every selection, because it too describes the render.
    resending: Repeat,
    /// What the turn about to be built is told about the authority it is
    /// running under, when `luu serve` set one — see
    /// [`Self::note_system`]. Not `self.system` itself: that field is the
    /// stable prefix a construction fixes for the session's life, and this one
    /// is set fresh, before every `select`, from whatever the caller's current
    /// authority and config say right now. `None` is *nothing to add*, not
    /// *forgot to set it* — a session that never calls `note_system` renders
    /// exactly as one from before this field existed.
    authority_note: Option<String>,
    /// The session's jobs, in order. Closed ones fold their turns at
    /// selection time; nothing here rewrites the history.
    jobs: Vec<Job>,
    /// Every plan that was put up, and what answered it, in order.
    ///
    /// Where `JobState::Proposed` and `JobState::Rejected` went. A plan is
    /// offered inside a draft and an **id is what approval hands out**, so a
    /// refused plan is not a job in a state — it is a round, and how many
    /// rounds a plan takes before it is approved is the number §seventh kept
    /// `Rejected` for. Kept beside the jobs rather than on one, because a round
    /// can be answered before any draft has opened: the decline is what runs
    /// the held prompt that opens it.
    rounds: Vec<Round>,
}

/// What an approval did: the job it opened, and the draft it closed on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approved {
    /// The job the approved plan opened. Turns land here from now on.
    pub job: JobId,
    /// The draft this approval closed and what it folded to, when a draft was
    /// open to close. `None` on the happy path — propose, approve, work — where
    /// no turn ever landed in the draft, so no draft was ever opened.
    pub folded: Option<(JobId, String)>,
}

/// What a reconstructed draft is called, so a reader of a resumed window can
/// tell it from one a person's prompt opened.
pub const RESTORED_DRAFT: &str =
    "turns resumed from a recording written before every turn had a job";

/// A plan put up at the gate, and what a person answered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Round {
    /// What the job would have been called, as the proposal named it.
    pub objective: String,
    /// The plan as it was put up — never as it was amended at the gate. The
    /// amendment belongs to the approval and is on the job it opened; keeping
    /// the proposal here is what makes the difference between them countable,
    /// which is [`crate::api::JobView::proposed`]'s whole argument one
    /// structure along.
    pub plan: Plan,
    /// Whether a planning call produced it, or the model answered in prose.
    pub source: crate::job::PlanSource,
    /// What answered it, or `None` while it is still on the table — which a
    /// live session never holds (the pending proposal lives beside the session,
    /// not in its history) and a recording that stops mid-gate does.
    #[serde(default)]
    pub answer: Option<Answer>,
}

/// The two answers a proposal can get. The negative one ends nothing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Answer {
    /// Approved: it closed whatever draft was open and opened this job.
    Approved { job: JobId },
    /// Declined — or *more changes*, which is the same answer. Nothing closed,
    /// nothing folded, and the conversation continued in the draft.
    Declined,
}

impl Context {
    pub fn new(system: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            tools: String::new(),
            tools_native: false,
            map: String::new(),
            turns: Vec::new(),
            floor: 0,
            pruned: 0,
            cited: Results::Kept,
            resending: Repeat::Always,
            authority_note: None,
            jobs: Vec::new(),
            rounds: Vec::new(),
        }
    }

    /// Adds the tool definitions to the prefix. Byte-stable by construction —
    /// see [`crate::tools::Tools::definitions`].
    pub fn with_tools(mut self, tools: impl Into<String>) -> Self {
        self.tools = tools.into();
        self.tools_native = false;
        self
    }

    /// The tools under the native transport: `specs` is their JSON as the
    /// request sends it, counted in the `tools` bucket and never rendered into
    /// the system message. See [`Self::tools_native`].
    pub fn with_native_tools(mut self, specs: impl Into<String>) -> Self {
        self.tools = specs.into();
        self.tools_native = true;
        self
    }

    pub fn tools(&self) -> &str {
        &self.tools
    }

    /// The tool specs the request carries beside the prompt, under the native
    /// transport; `None` when the tools are rendered into the system message
    /// or there are none.
    pub fn native_tools(&self) -> Option<&str> {
        (self.tools_native && !self.tools.is_empty()).then_some(self.tools.as_str())
    }

    /// Adds the repository outline to the prefix, under the tool definitions.
    pub fn with_map(mut self, map: impl Into<String>) -> Self {
        self.map = map.into();
        self
    }

    pub fn map(&self) -> &str {
        &self.map
    }

    pub fn floor(&self) -> usize {
        self.floor
    }

    /// The prune line. Turns below it are still in the window and still
    /// answered; what they no longer carry is their code.
    pub fn pruned(&self) -> usize {
        self.pruned
    }

    /// Reconstructs an active context from a stored session view — the inverse fold.
    ///
    /// Rebuilds turns, jobs, summaries and eviction floor from the view,
    /// counting tokens with the supplied counter so the resumed context
    /// budgets identically to the original session.
    ///
    /// **`sandbox` is how a turn gets its code back**, and the only way it can:
    /// the view carries each turn's spans by reference, so they are *read
    /// again* here, through whatever this resume is allowed to read rather than
    /// through whatever the original session was. `None` restores none of them
    /// and is what a caller with no filesystem in hand gets. A span that cannot
    /// be read comes back as a citation and is named in
    /// [`Restored::unreadable`], because a resume that quietly restores four
    /// spans of six is a refusal nobody can read.
    ///
    /// The re-read happens once, here, and not at every render: `Context` holds
    /// no sandbox and this is the one moment where the caller already does. So
    /// a resumed session's window is fresh at the moment it is resumed and goes
    /// stale again from the next turn exactly as a live one does — see
    /// `RECORD/2026-09-19.fragments-by-reference.completed.md`, which says so
    /// rather than implying the defect is fixed.
    pub fn from_view(
        view: &crate::api::SessionView,
        system: impl Into<String>,
        tools: impl Into<String>,
        map: impl Into<String>,
        counter: &dyn TokenCounter,
        sandbox: Option<&crate::sandbox::Sandbox>,
    ) -> Restored {
        let mut jobs = Vec::with_capacity(view.jobs.len());
        for jv in &view.jobs {
            let summary = jv.summary.as_ref().map(|text| crate::job::Summary {
                text: text.clone(),
                tokens: counter.count(text),
                counted_by: counter.id(),
                // Carried rather than recomputed: what the fold replaced was
                // counted at the close, over turns this view may no longer
                // have. Recounting it here would answer a different question
                // and look like the same one.
                replaced: jv.replaced.clone(),
            });
            jobs.push(Job {
                id: jv.id,
                objective: jv.objective.clone(),
                plan: jv.plan.clone(),
                state: jv.state,
                summary,
                closed_by: jv.closed_by,
                approved_by: jv.approved_by.clone(),
            });
        }

        let mut turns = Vec::with_capacity(view.turns.len());
        let mut unreadable = Vec::new();
        // Made once and only if a jobless turn turns up, so a recording written
        // under the alternation gains nothing at all.
        let mut restored: Option<JobId> = None;
        for tv in &view.turns {
            let mut steps = Vec::with_capacity(tv.tools.len());
            for call_view in &tv.tools {
                let call = crate::tools::ToolCall {
                    name: call_view.name.clone(),
                    arguments: call_view.arguments.clone(),
                };
                let text = format!(
                    "```tool\n{}\n```",
                    serde_json::to_string(&call).unwrap_or_default()
                );
                let outcome = crate::tools::ToolOutcome {
                    verdict: call_view.verdict.clone().unwrap_or_else(|| {
                        crate::sandbox::Verdict::allow("resumed", crate::sandbox::Applied::Process)
                    }),
                    output: call_view.output.clone(),
                    error: call_view.error.clone(),
                    truncated: call_view.truncated,
                    command: call_view.command.clone(),
                };
                steps.push(ToolStep {
                    text,
                    call,
                    outcome,
                    duration_ms: call_view.duration_ms.unwrap_or(0),
                    asked: call_view.asked,
                });
            }

            // Read again, never restored from bytes: there are none in the
            // view, on purpose. A stream written before protocol 6 says a turn
            // had no spans, which is indistinguishable from a turn that had
            // none — and is why the format number is what tells a file that
            // cannot say from a session that had nothing to say.
            let mut code_context = Vec::with_capacity(tv.code.len());
            for grounding in &tv.code {
                let Some(sandbox) = sandbox else { continue };
                let spec = crate::fragment::Spec::parse(&grounding.spec);
                match crate::fragment::load(sandbox, &spec) {
                    Ok(fragment) => code_context.push(fragment),
                    Err(error) => {
                        code_context.push(unreadable_fragment(&grounding.spec, &error));
                        unreadable.push(Unreadable {
                            turn: tv.turn,
                            spec: grounding.spec.clone(),
                            why: error.to_string(),
                        });
                    }
                }
            }

            let prompt = tv.prompt.clone();
            let answer = tv.text.clone();
            let tokens = counter.count(&prompt)
                + fragments_tokens(&code_context.iter().collect::<Vec<_>>(), counter)
                + steps_tokens(&steps, counter)
                + counter.count(&answer);

            // Where the claim is made, or refused. `None` on the wire means
            // *written before jobs were total*, which is emphatically not the
            // same claim as *this turn was in a draft* — so the turns are given
            // one reconstructed draft between them and it says so in its
            // objective, rather than being quietly folded into a job they were
            // never asked inside.
            let job = match tv.job {
                Some(job) => job,
                None => *restored.get_or_insert_with(|| {
                    let id = jobs.iter().map(|job| job.id).max().unwrap_or(0) + 1;
                    // At the front, because these turns come before every job
                    // the view carries, and open, because nothing ever folded
                    // them and a resume must not invent a summary that was
                    // never written. It is therefore an open job that is not
                    // `current_job` — the one place the alternation's *at most
                    // one job is open* does not hold, and it holds for every
                    // stream written under it.
                    jobs.insert(0, Job::draft(id, RESTORED_DRAFT));
                    id
                }),
            };

            turns.push(Turn {
                id: tv.turn,
                prompt,
                answer,
                steps,
                code_context,
                job,
                tokens,
                counted_by: counter.id(),
            });
        }

        let floor = view
            .turns
            .iter()
            .take_while(|t| t.evicted_by.is_some())
            .count();

        Restored {
            context: Self {
                system: system.into(),
                tools: tools.into(),
                // The caller's to set with `with_native_tools`: which transport
                // a resumed session sends under is its destination's, and the
                // view does not say.
                tools_native: false,
                map: map.into(),
                turns,
                floor,
                // Zero and not carried, and this stays true now that the spans
                // come back: the prune line is a fact about the *last render*,
                // and a resumed session has not rendered. Restoring a line
                // here would say a turn had given up spans that are, as of
                // this moment, in its `code_context`.
                pruned: 0,
                // The last render's rule, and a resumed session has not
                // rendered: the next selection sets it from the budget it is
                // given.
                cited: Results::Kept,
                resending: Repeat::Always,
                // A resumed session has not rendered either, [`Self::cited`]'s
                // reason: the next `select` sets it fresh from whatever the
                // caller's authority is *now*, which may not be what it was
                // when the recording was made.
                authority_note: None,
                jobs,
                // Carried, because a round is history and not a render: how
                // many times a plan was put up before one was approved is a
                // fact about the session, and a resume that dropped it would
                // make every resumed session look like a first-round approval.
                rounds: view.rounds.clone(),
            },
            unreadable,
        }
    }

    /// The whole cached prefix, as one message. Assembled here and nowhere else
    /// so that two call sites cannot join it differently.
    ///
    /// [`Self::authority_note`] renders last, after the map, on purpose: the
    /// map and the tools describe the repository and its rest and do not
    /// change with the turn's authority, while the note is the one part of
    /// this block that can — a draft becoming a plan changes what follows it
    /// here exactly as opening a job already changes the turns below.
    fn system_message(&self) -> String {
        let mut text = self.system.clone();
        // Under the native transport the server's template renders the tools,
        // with the tool prompt the model was trained on; ours stays out.
        let tools = (!self.tools_native).then_some(&self.tools);
        for block in tools.into_iter().chain([&self.map]) {
            if !block.is_empty() {
                text.push_str("\n\n");
                text.push_str(block);
            }
        }
        if let Some(note) = &self.authority_note {
            text.push_str("\n\n");
            text.push_str(note);
        }
        text
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn system(&self) -> &str {
        &self.system
    }

    /// Sets what the *next* `select` tells the model about the authority it
    /// is running under, under `position = "system"` — `position = "prompt"`
    /// never reaches this far in, because it is the caller's own prompt
    /// string that carries it. `None` clears whatever was set before: an
    /// authority note is a fact about the turn about to render, never one
    /// carried over from the turn before it, so a caller that stops setting
    /// one is a caller whose note goes away on its very next call, the same
    /// turn the authority itself would.
    pub fn note_system(&mut self, note: Option<String>) {
        self.authority_note = note;
    }

    /// Closes a turn and counts it, once. `id` is the number the session gave
    /// the turn — see [`Turn::id`].
    pub fn push_turn(
        &mut self,
        id: TurnId,
        prompt: impl Into<String>,
        answer: impl Into<String>,
        code_context: Vec<Fragment>,
        counter: &dyn TokenCounter,
    ) {
        self.push_turn_with_steps(id, prompt, answer, code_context, Vec::new(), counter);
    }

    /// The same, for a turn in which the agent used tools.
    pub fn push_turn_with_steps(
        &mut self,
        id: TurnId,
        prompt: impl Into<String>,
        answer: impl Into<String>,
        code_context: Vec<Fragment>,
        steps: Vec<ToolStep>,
        counter: &dyn TokenCounter,
    ) {
        let prompt = prompt.into();
        let answer = answer.into();
        let tokens = counter.count(&user_text(&code_context, &prompt))
            + steps_tokens(&steps, counter)
            + counter.count(&answer);
        // Where the draft is born. A turn with nowhere to land opens one and
        // lands in it, which is the derivation that answers both of §eighth's
        // leftovers at once: a draft nobody asked anything inside never exists,
        // so it cannot fold to an empty summary, and a reopen is unambiguous
        // exactly when nothing has been asked since the close.
        //
        // The objective is the prompt as the user typed it, because that is
        // what a draft's objective *is* — the plan is the part nobody has
        // written yet.
        let job = match self.current_job() {
            Some(job) => job,
            None => self.open_draft(prompt.clone()),
        };
        self.turns.push(Turn {
            id,
            prompt,
            answer,
            steps,
            code_context,
            job,
            tokens,
            counted_by: counter.id(),
        });
    }

    /// Opens a draft to hold a turn that has nowhere else to go.
    ///
    /// Private on purpose: **a job opens when a turn lands in it**, so the only
    /// caller is [`Self::push_turn_with_steps`]. Exposing it would let a caller
    /// open a draft that never receives a turn, which is exactly the zero-turn
    /// fold this rule exists to make unreachable.
    fn open_draft(&mut self, objective: impl Into<String>) -> JobId {
        let id = self.next_job_id();
        self.jobs.push(Job::draft(id, objective));
        id
    }

    /// Ids are handed out by position and never reused, rejections included —
    /// except that a rejection no longer takes one, which is what retired the
    /// reason `Rejected` was kept as a state.
    pub fn next_job_id(&self) -> JobId {
        self.jobs.len() as JobId + 1
    }

    /// The job turns are currently attributed to: the last one not closed.
    ///
    /// **Derived, never stored.** A field beside the jobs would disagree with
    /// them the first time a resume rebuilt one and not the other, which is
    /// [`crate::api::SessionView::counts`]'s argument taken as read. `None` is
    /// a session whose last job is closed (or which has none yet), and the next
    /// turn opens a draft rather than finding one.
    pub fn current_job(&self) -> Option<JobId> {
        self.jobs
            .last()
            .filter(|job| job.is_open())
            .map(|job| job.id)
    }

    /// Approves a plan: **closes whatever draft is open, and opens the job that
    /// plan describes.**
    ///
    /// Approving *is* closing, which is the sentence the whole alternation rests
    /// on — see `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`
    /// §seventh. The draft folds here and its summary becomes the new job's
    /// context: *this is what we looked at, this is what we decided, this is
    /// what we are now doing*, in that order and in one window.
    ///
    /// `plan` is the plan **as approved** — amended at the gate and checked
    /// against the policy file before it arrives here. `proposed` is what the
    /// model put up, kept in the round so the difference between them stays
    /// countable.
    pub fn approve_plan(
        &mut self,
        objective: impl Into<String>,
        proposed: Plan,
        plan: Plan,
        source: crate::job::PlanSource,
        by: ApprovedBy,
        counter: &dyn TokenCounter,
    ) -> Approved {
        let objective = objective.into();
        // The draft closes first, so the id arithmetic below counts it. A draft
        // that never received a turn was never opened and there is nothing to
        // fold — which is the happy path, and why `folded` is an `Option`.
        let folded = match self.current_job() {
            // `Approval` and not `User`: a person clicking *close & fold* and a
            // plan being approved out of a draft are different rungs, and under
            // the alternation the second is the common one. See
            // `RECORD/2026-09-21.the-alternation-on-the-page.completed.md`.
            Some(draft) => self
                .close_job_by(draft, counter, ClosedBy::Approval)
                .map(|summary| (draft, summary)),
            None => None,
        };
        let id = self.next_job_id();
        self.jobs
            .push(Job::planned(id, objective.clone(), plan, by));
        self.rounds.push(Round {
            objective,
            plan: proposed,
            source,
            answer: Some(Answer::Approved { job: id }),
        });
        Approved { job: id, folded }
    }

    /// Refuses a proposal. Nothing closes and nothing folds — *decline* and
    /// *more changes* are the same answer, **not yet**, and both keep you in
    /// the job you are in.
    ///
    /// The prompt that was held runs after this rather than being dropped,
    /// which is the one behaviour a person can observe changing in item 22: it
    /// is what opens the draft the conversation continues in.
    pub fn decline_plan(
        &mut self,
        objective: impl Into<String>,
        plan: Plan,
        source: crate::job::PlanSource,
    ) {
        self.rounds.push(Round {
            objective: objective.into(),
            plan,
            source,
            answer: Some(Answer::Declined),
        });
    }

    /// Every plan put up at the gate, in order, and what answered it.
    pub fn rounds(&self) -> &[Round] {
        &self.rounds
    }

    /// Closes it because a person said so. The rung below
    /// [`Context::close_if_met`], and the only one there was until it existed.
    pub fn close_job(&mut self, id: JobId, counter: &dyn TokenCounter) -> Option<String> {
        self.close_job_by(id, counter, ClosedBy::User)
    }

    pub fn close_task(&mut self, id: TaskId, counter: &dyn TokenCounter) -> Option<String> {
        self.close_job(id, counter)
    }

    /// Closes the live job if its plan's `closes_on` has been met by its own
    /// steps, and returns what closed and what it folded to.
    ///
    /// Called after a turn is folded into the history, which is what makes the
    /// close see that turn's steps. Nothing happens for a job whose plan
    /// declares no closing condition, which is every job a model planned on
    /// its own — see `RECORD/2026-09-02.closing-on-an-exit-code.completed.md` for why
    /// the field arrives at the gate and never from the planning call.
    pub fn close_if_met(&mut self, counter: &dyn TokenCounter) -> Option<(JobId, String)> {
        let id = self.live_job()?;
        let mine = |turn: &&Turn| turn.job == id;
        let steps: Vec<&ToolStep> = self
            .turns
            .iter()
            .filter(mine)
            .flat_map(|turn| turn.steps.iter())
            .collect();
        let job = self.jobs.iter().find(|job| job.id == id)?;
        // A draft has no plan, so it has no closing condition and nothing here
        // can fire for one. That is the rung staying where it was: `closes_on`
        // arrives at the gate, and a job that never went through the gate never
        // acquired one.
        let Some(plan) = &job.plan else {
            return None;
        };
        if !plan.met_by(&steps) {
            return None;
        }
        let summary = self.close_job_by(id, counter, ClosedBy::ExitCode)?;
        Some((id, summary))
    }

    /// Closes it: from the next selection on, its turns render as one summary.
    ///
    /// The summary is written from the job's own tool steps and the fragments
    /// its turns were handed, so it is evidence rather than the model's account
    /// of itself. Returns the summary text, or `None` if there is no such job.
    pub fn close_job_by(
        &mut self,
        id: JobId,
        counter: &dyn TokenCounter,
        by: ClosedBy,
    ) -> Option<String> {
        let mine = |turn: &&Turn| turn.job == id;
        let steps: Vec<&ToolStep> = self
            .turns
            .iter()
            .filter(mine)
            .flat_map(|turn| turn.steps.iter())
            .collect();
        // The field beside the one above, and the reason the fold lost answers
        // until now: what a job read was in hand at the close and was never
        // asked for. See `RECORD/2026-08-30.the-fold-probe-run.completed.md`.
        let shown: Vec<&Fragment> = self
            .turns
            .iter()
            .filter(mine)
            .flat_map(|turn| turn.code_context.iter())
            .collect();
        let turns = self.turns.iter().filter(mine).count();
        // What the fold is about to stop sending, counted here rather than
        // afterwards: the window moves, a resume can change the counter, and
        // eviction may already have taken some of the same turns, so this
        // number is only true at the close. Counted through the accounting the
        // `history` bucket sums rather than through the stored count, which are
        // the same number for a turn above the prune line and are not for one
        // below it: a pruned turn is in the prompt as its citations, so a fold
        // that claimed its spans back would be claiming a saving rule B had
        // already taken. That divergence arrived with rule B on 2026-09-08 and
        // is fixed here. See
        // `RECORD/2026-09-08.what-a-fold-writes-down.completed.md` and
        // `RECORD/2026-09-09.what-a-result-costs.completed.md`.
        // No asking turn: a fold happens between renders, so what the session
        // knows about a path is what its turns say. Under `Repeat::Always` this
        // is empty and the arithmetic below is unchanged, which is the
        // property every one of rule A's costs is built to keep.
        let current = Current::of(&self.turns, &[], self.resending);
        let replaced = Replaced {
            turns: self.turns.iter().filter(mine).map(|turn| turn.id).collect(),
            tokens: self
                .turns
                .iter()
                .enumerate()
                .filter(|(_, turn)| turn.job == id)
                .map(|(index, _)| {
                    self.item_tokens(
                        &Item::Turn(index),
                        counter,
                        self.pruned,
                        self.cited,
                        &current,
                    )
                })
                .sum(),
            // Filled by the close, which is where the summary is written and
            // therefore the only place its own count exists.
            summary_tokens: 0,
        };
        let job = self.jobs.iter_mut().find(|job| job.id == id)?;
        // Only an open job folds. A proposal has no turns to fold and nothing
        // has been approved to summarise; closing one would take the gate off
        // the screen with its prompt still held, and the session would be stuck
        // with no way to answer a proposal nobody can see.
        if !job.is_open() {
            return None;
        }
        job.close(&steps, &shown, turns, counter, by, Some(replaced));
        job.summary.as_ref().map(|summary| summary.text.clone())
    }

    /// What the fold of a closed job replaced, for the message that announces
    /// it. Read back rather than returned by the close: the close's answer is
    /// the summary a person reads, and threading a second value through four
    /// call sites to save one lookup is how a signature grows.
    pub fn replaced_by(&self, id: JobId) -> Option<Replaced> {
        self.job(id)
            .and_then(|job| job.summary.as_ref())
            .and_then(|summary| summary.replaced.clone())
    }

    pub fn close_task_by(
        &mut self,
        id: TaskId,
        counter: &dyn TokenCounter,
        by: ClosedBy,
    ) -> Option<String> {
        self.close_job_by(id, counter, by)
    }

    /// Reopens it: the fold stops applying and its turns are sent verbatim
    /// again. Nothing is recovered, because nothing was deleted.
    ///
    /// **Only the last job, and only while nothing is open after it.** Under an
    /// alternation any other reopen is ambiguous the moment anything has
    /// happened since: reopening job 2 while 3 and 4 exist either gives the
    /// session two open jobs or obliges the reopen to close them, and neither
    /// is a thing anyone asked for. The case that survives is the one where
    /// there is nothing after the close to conflict with — which is the same
    /// derivation that kills the zero-turn draft, taken from the other end.
    pub fn reopen_job(&mut self, id: JobId) -> bool {
        if self.jobs.last().map(|job| job.id) != Some(id) {
            return false;
        }
        match self.job_mut(id) {
            Some(job) if job.is_closed() => {
                job.reopen();
                true
            }
            _ => false,
        }
    }

    pub fn reopen_task(&mut self, id: TaskId) -> bool {
        self.reopen_job(id)
    }

    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    pub fn tasks(&self) -> &[Job] {
        &self.jobs
    }

    pub fn job(&self, id: JobId) -> Option<&Job> {
        self.jobs.iter().find(|job| job.id == id)
    }

    pub fn task(&self, id: TaskId) -> Option<&Job> {
        self.job(id)
    }

    fn job_mut(&mut self, id: JobId) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    /// The job turns are currently attributed to. One level, deliberately —
    /// see the jobs record.
    ///
    /// The older name for [`Self::current_job`], and the same answer: at most
    /// one job is open, everything behind it is a fold. It used to scan for the
    /// last open job because several could be open at once; under the
    /// alternation only the last one can be, and a scan that can only find the
    /// last element is a scan that hides its own invariant.
    pub fn live_job(&self) -> Option<JobId> {
        self.current_job()
    }

    pub fn live_task(&self) -> Option<TaskId> {
        self.live_job()
    }

    /// The history as it renders: live turns one by one, and each closed job
    /// as a single folded item.
    ///
    /// Built from the floor forward, so a job closed after part of it was
    /// evicted folds only from what the window still holds — except that the
    /// summary it folds to is the whole job's. That is deliberate and it is a
    /// cost: see `RECORD/2026-08-30.tasks-in-code.completed.md`.
    fn items(&self) -> Vec<Item> {
        self.items_from(self.floor)
    }

    /// The same, from a floor that is not (yet) the one in force — the
    /// eviction report needs the window as it stood before the cut, and that
    /// floor is gone by the time there is anything to report.
    fn items_from(&self, floor: usize) -> Vec<Item> {
        let mut items = Vec::new();
        let mut index = floor;
        while index < self.turns.len() {
            let folded =
                Some(self.turns[index].job).filter(|id| self.job(*id).is_some_and(Job::is_closed));
            match folded {
                Some(id) => {
                    let first = index;
                    while index < self.turns.len() && self.turns[index].job == id {
                        index += 1;
                    }
                    items.push(Item::Folded { job: id, first });
                }
                None => {
                    items.push(Item::Turn(index));
                    index += 1;
                }
            }
        }
        items
    }

    /// What an item costs in the prompt it renders into, under a prune line
    /// that may or may not be the one in force — deciding needs the cost of a
    /// line the context has not taken yet.
    ///
    /// A pruned turn is the stored count less the spans it will not send, plus
    /// the citations that stand in for them, and — under [`Results::Cited`] —
    /// less its tool output and plus the lines that cite that. Subtracted
    /// rather than recounted, which is rule A's rule for rule A's reason: a
    /// turn that gives up nothing costs exactly what it has always cost, and
    /// every recording made before this reads unchanged.
    ///
    /// A turn *above* the line pays the same arithmetic for a different reason:
    /// a span whose bytes a later turn has read differently goes out as a
    /// citation too. `current` is empty under [`Repeat::Always`], so that arm
    /// is the stored count exactly as it always was.
    fn item_tokens(
        &self,
        item: &Item,
        counter: &dyn TokenCounter,
        pruned: usize,
        results: Results,
        current: &Current,
    ) -> u32 {
        match item {
            Item::Turn(index) => {
                let turn = &self.turns[*index];
                let stored = self.tokens_of(turn, counter);
                match *index < pruned {
                    // Above the line the turn sends its spans, except the ones
                    // the session has since read differently: those are the
                    // line that cites them, and the same subtract-and-add-back
                    // this function does one arm along.
                    false => turn
                        .code_context
                        .iter()
                        .filter(|fragment| current.superseded(fragment))
                        .fold(stored, |total, fragment| {
                            total.saturating_sub(fragment_tokens(fragment, counter))
                                + superseded_fragment_tokens(fragment, counter)
                        }),
                    true => {
                        let carried = turn.code_context.iter();
                        let full: u32 = carried
                            .clone()
                            .map(|fragment| fragment_tokens(fragment, counter))
                            .sum();
                        let cited: u32 = carried
                            .map(|fragment| pruned_fragment_tokens(fragment, counter))
                            .sum();
                        let stored = stored.saturating_sub(full) + cited;
                        match results {
                            // Left as its own arm rather than folded into the
                            // one below, which would compute the same number by
                            // subtracting a total and adding it back: identical
                            // in arithmetic, and not identical where the stored
                            // count is smaller than the results it contains,
                            // because the subtraction saturates.
                            Results::Kept => stored,
                            Results::Cited | Results::CitedReads => {
                                stored.saturating_sub(results_tokens(&turn.steps, counter))
                                    + sent_results_tokens(&turn.steps, results, counter)
                            }
                        }
                    }
                }
            }
            Item::Folded { job, .. } => self
                .job(*job)
                .and_then(|job| {
                    job.summary.as_ref().map(|summary| {
                        // Stored counts are stale under a changed counter, the
                        // same way a turn's are, and are redone rather than
                        // summed into a different unit.
                        match summary.counted_by == counter.id() {
                            true => summary.tokens,
                            false => counter.count(&summary.text),
                        }
                    })
                })
                .unwrap_or(0),
        }
    }

    /// Chooses what fits, then renders it.
    ///
    /// History is kept from the newest backwards and always in whole turns, so
    /// the retained window can only ever start on a user message. Half a turn
    /// would leave an answer to a question nobody asked, and a window starting
    /// on an assistant message makes several chat templates continue instead of
    /// answering.
    ///
    /// Takes `&mut self` because evicting is not a reading of the history: what
    /// leaves the window stays out, so the decision has to outlive the call
    /// that made it. See [`Context::floor`].
    pub fn select(
        &mut self,
        prompt: &str,
        code_context: &[Fragment],
        budget: Budget,
        counter: &dyn TokenCounter,
    ) -> Selection {
        self.cited = budget.results;
        self.resending = budget.repeat;
        // Before anything is decided, because every cost below is a cost of the
        // render and the render sends a superseded span as its citation. Owned
        // rather than borrowed for the reason [`Current`] gives: the floor and
        // the prune line move between here and the walk.
        let current = Current::of(&self.turns, code_context, budget.repeat);
        // The note counts with the block it renders in, under `position =
        // "system"` — the system bucket is where `system_message` puts it,
        // and a bucket that did not count what it sends is the thing this
        // project's whole counting discipline exists to refuse.
        let system_tokens = counter.count(&self.system)
            + self.authority_note.as_deref().map_or(0, |note| {
                // `system_message` joins with two newlines the same way this
                // adds a separate count rather than concatenating first: the
                // two can differ by a token or two across the join, the same
                // order this file already accepts for the tools/map split
                // below, and re-tokenizing the whole joined string on every
                // turn just to save that token or two is not the trade.
                counter.count(note)
            });
        let tools_tokens = counter.count(&self.tools);
        let map_tokens = counter.count(&self.map);
        // Counted as the two buckets they will be plotted as. Tokenization is
        // not additive across a boundary, so this can differ by a token or two
        // from counting the concatenation; that is the same order as the
        // template overhead we already accept and report.
        //
        // Under `Repeat::Once` this is the whole bucket and not what will be
        // sent: what the turn gives up to the history depends on where the
        // floor lands, and the floor is decided below using this number. The
        // fixed point is not worth solving, because the direction is the safe
        // one — the budget the cut is computed against is the same one it would
        // have had without the rule, so the cut is never deeper than the
        // default's and the prompt that goes out is never bigger than the one
        // it was sized for. The saving that pays is the history's, below.
        let code_tokens = counter.count(&fragments_text(code_context));
        let prompt_tokens = counter.count(prompt);

        let mut eviction = None;
        let mut pruning = None;
        if let Some(limit) = budget.limit {
            // The current turn and the reserve are not negotiable: nothing here
            // may trim the prompt the user just typed. If they alone exceed the
            // window, history is empty and the turn still goes — being over the
            // limit is the backend's error to report, not ours to hide by
            // cutting the question in half.
            let fixed = system_tokens
                + tools_tokens
                + map_tokens
                + code_tokens
                + prompt_tokens
                + budget.reserve;
            let available = limit.saturating_sub(fixed);

            // How deep the cut goes is the whole difference between the two
            // policies, and the prune line goes to the same place for the same
            // reason: a second constant would be that argument had again,
            // worse.
            let target = match budget.eviction {
                Eviction::Turn => available,
                Eviction::Block { low_water } => {
                    (available as f32 * low_water.clamp(0.0, 1.0)) as u32
                }
            };

            // Pruning first, because it is the cheaper half of the same trade:
            // an evicted turn loses its question and its answer with its code,
            // and a pruned one loses only the code. The floor moves after, and
            // only if giving up every span in the window still leaves it over.
            if budget.prune == Prune::Behind
                && self.fits_from(
                    available,
                    counter,
                    budget.repeat,
                    self.pruned,
                    budget.results,
                    &current,
                ) > self.floor
            {
                let before = self.pruned;
                let held = self.window_tokens(
                    self.floor,
                    counter,
                    budget.repeat,
                    before,
                    budget.results,
                    &current,
                );
                self.pruned =
                    self.prune_to(target, counter, budget.repeat, budget.results, &current);
                pruning = (self.pruned > before).then(|| Pruned {
                    // A turn is named here because it gave something up, and
                    // under `Results::Cited` there is a second thing to give:
                    // a turn that ran a tool and selected nothing is behind the
                    // line for a reason the report has to be able to state.
                    turns: self.turns[before.max(self.floor)..self.pruned]
                        .iter()
                        .filter(|turn| {
                            !turn.code_context.is_empty()
                                || (budget.results != Results::Kept
                                    && turn.steps.iter().any(|step| {
                                        result_under(step, budget.results, counter)
                                            != step.result_text()
                                    }))
                        })
                        .map(|turn| turn.id)
                        .collect(),
                    tokens: held.saturating_sub(self.window_tokens(
                        self.floor,
                        counter,
                        budget.repeat,
                        self.pruned,
                        budget.results,
                        &current,
                    )),
                    counter: counter.id(),
                });
            }

            if self.fits_from(
                available,
                counter,
                budget.repeat,
                self.pruned,
                budget.results,
                &current,
            ) > self.floor
            {
                // Taken before the floor moves: this is the window the cut
                // chooses from, and what it drops is unreachable afterwards —
                // which is the whole reason this has to be reported from in
                // here rather than reconstructed outside. Under the prune line
                // already in force, so that what pruning saved is not reported
                // as what the cut freed.
                let before = self.floor;
                let held = self.window_tokens(
                    before,
                    counter,
                    budget.repeat,
                    self.pruned,
                    budget.results,
                    &current,
                );

                self.floor = self.fits_from(
                    target,
                    counter,
                    budget.repeat,
                    self.pruned,
                    budget.results,
                    &current,
                );

                // What the cut freed, and not what the dropped turns were
                // counted at. Under `Repeat::Once` those are two different
                // numbers: a span the oldest turn owned passes to the next turn
                // that carries it rather than leaving with it, so summing the
                // items below the floor would report a saving the window did
                // not get. The difference of two windows is the saving.
                eviction = (self.floor > before).then(|| Evicted {
                    turns: self.turns[before..self.floor]
                        .iter()
                        .map(|turn| turn.id)
                        .collect(),
                    tokens: held.saturating_sub(self.window_tokens(
                        self.floor,
                        counter,
                        budget.repeat,
                        self.pruned,
                        budget.results,
                        &current,
                    )),
                    counter: counter.id(),
                    policy: budget.eviction,
                });
            }
        }

        // Decided, then rendered — and the fold is part of the decision: a
        // closed task is one item worth its summary, so eviction and compaction
        // compose instead of arguing over the same turns.
        let items = self.items();
        let mut history_tokens = 0;
        let mut summary_tokens = 0;
        let mut messages = Vec::with_capacity(items.len() * 2 + 2);
        messages.push(Message::system(self.system_message()));
        // Oldest first, so the first turn that carries a span is the one that
        // renders it — and the turn being asked, which is rendered last, is the
        // only one that can lose a span to the history. That direction is what
        // keeps the prefix: the block above the newest message is byte-identical
        // to the one the previous call sent, exactly as it was before this rule.
        let mut shown: HashSet<&Fragment> = HashSet::new();
        // What actually went out, per path, in window order. Recorded beside
        // the render rather than derived from `messages` afterwards, because
        // parsing back the text we just wrote is the thing `code_context` is
        // stored separately to avoid.
        // `None` is the turn being asked, which has no id yet.
        let mut sent: Vec<(&str, &str, Option<TurnId>)> = Vec::new();
        // What rule A kept out, accumulated as the render walks. Collected here
        // rather than recomputed afterwards for `sent`'s own reason: the answer
        // is a by-product of the walk, and deriving it again would be a second
        // implementation of the rule sitting beside the rule.
        let mut repeated = Repeated {
            turns: Vec::new(),
            asking: false,
            spans: 0,
            tokens: 0,
            counter: counter.id(),
        };
        // What went out as a citation because the session has read it again
        // since. Accumulated on the walk for `sent`'s own reason: the answer is
        // a by-product of rendering, and deriving it afterwards would be a
        // second implementation of the rule sitting beside the rule.
        let mut superseded: Vec<Superseded> = Vec::new();
        for item in &items {
            let tokens = self.item_tokens(item, counter, self.pruned, budget.results, &current);
            match item {
                Item::Turn(index) if *index < self.pruned => {
                    // Below the prune line: the exchange stays, the code does
                    // not, and what stands where it stood is the line that
                    // cites it. `item_tokens` has already charged this turn for
                    // the citations, so there is nothing to subtract here — a
                    // pruned turn owns none of its spans and hands every one of
                    // them to the oldest turn above the line that carries it.
                    let turn = &self.turns[*index];
                    history_tokens += tokens;
                    messages.push(Message::user(pruned_user_text(
                        &turn.code_context,
                        &turn.prompt,
                        counter,
                    )));
                    // The call is the model's own words and stays verbatim;
                    // what a pruned turn gives up is what came back. Under
                    // `Results::Kept` this is the same loop the turn above it
                    // runs, which is what keeps a run made before the rule
                    // byte-identical under it.
                    for step in &turn.steps {
                        messages.push(Message::assistant(step.text.clone()));
                        messages.push(Message::user(result_under(step, budget.results, counter)));
                    }
                    messages.push(Message::assistant(turn.answer.clone()));
                }
                Item::Turn(index) => {
                    let turn = &self.turns[*index];
                    let (kept, dropped) =
                        split_shown(&turn.code_context, &mut shown, budget.repeat);
                    // The stored count, less what this render will not send.
                    // Subtracted rather than recounted so that a turn repeating
                    // nothing costs exactly what it has always cost, and every
                    // recording made before `Repeat` reads unchanged. What a
                    // dropped span would have cost *here* is its citation where
                    // the session has since read it differently, which is the
                    // same number `item_tokens` charged the turn above.
                    let unsent = shown_fragments_tokens(&dropped, &current, counter);
                    history_tokens += tokens.saturating_sub(unsent);
                    if !dropped.is_empty() {
                        repeated.turns.push(turn.id);
                        repeated.spans += dropped.len();
                        repeated.tokens += unsent;
                    }
                    // Bodies, and a citation is not one: a turn whose stale span
                    // was replaced sent no body for that path, so the detector
                    // one block down has nothing to report about it. That is
                    // the fix, stated where it takes effect.
                    for fragment in &kept {
                        match current.superseded(fragment) {
                            true => note_superseded(&mut superseded, fragment, turn.id, counter),
                            false => sent.push((&fragment.path, &fragment.text, Some(turn.id))),
                        }
                    }
                    messages.push(Message::user(shown_user_text(
                        kept,
                        &turn.prompt,
                        &current,
                        counter,
                    )));
                    // Each step is a real exchange, so the alternation holds and
                    // no chat template has to decide what two user messages in a
                    // row mean.
                    for step in &turn.steps {
                        messages.push(Message::assistant(step.text.clone()));
                        messages.push(Message::user(step.result_text()));
                    }
                    messages.push(Message::assistant(turn.answer.clone()));
                }
                Item::Folded { job, .. } => {
                    summary_tokens += tokens;
                    // One exchange, so the alternation a folded block sits in
                    // is the same one a turn would have left behind. The user
                    // half is the objective as it was approved: it is what was
                    // asked, and inventing a sentence for it would be prose in
                    // the one place this design keeps prose out of.
                    let Some(job) = self.job(*job) else {
                        continue;
                    };
                    let Some(summary) = &job.summary else {
                        continue;
                    };
                    messages.push(Message::user(job.objective.clone()));
                    messages.push(Message::assistant(summary.text.clone()));
                }
            }
        }
        // The turn being asked, last and youngest: what the history above it is
        // already showing, it does not show again. `code_tokens` above was
        // counted before the floor moved, so it is the whole bucket; this is
        // what of it is actually sent.
        let (kept, dropped) = split_shown(code_context, &mut shown, budget.repeat);
        let code_tokens = match kept.len() == code_context.len() {
            true => code_tokens,
            false => counter.count(&fragments_text(kept.iter().copied())),
        };
        // The one case where the *fresh* read of a span is the one dropped,
        // because an older turn in this same prompt is already showing it —
        // which is the direction `Repeat::Once` was chosen for and the reason
        // item 19's defect is invisible to it.
        if !dropped.is_empty() {
            repeated.asking = true;
            repeated.spans += dropped.len();
            repeated.tokens += shown_fragments_tokens(&dropped, &current, counter);
        }
        sent.extend(
            kept.iter()
                .map(|fragment| (&*fragment.path, &*fragment.text, None)),
        );
        messages.push(Message::user(user_text(kept, prompt)));

        let diverged = diverged_paths(&sent);

        let mut buckets = vec![
            Bucket::new("system", system_tokens),
            Bucket::new("tools", tools_tokens),
            // Beside the tools rather than inside them: both are prefix, and
            // "the prefix grew" is not an answer to why the window is full.
            Bucket::new("map", map_tokens),
            // Beside `history` rather than inside it: the fold is the one thing
            // the panel exists to watch, and a single bar cannot show a block
            // being replaced by a line.
            Bucket::new("summaries", summary_tokens),
            Bucket::new("history", history_tokens),
            Bucket::new("code", code_tokens),
            Bucket::new("prompt", prompt_tokens),
        ];
        // Headroom only means something against a known window.
        if budget.limit.is_some() {
            buckets.push(Bucket::new("reserve", budget.reserve));
        }

        Selection {
            messages,
            buckets,
            limit: budget.limit,
            counter: counter.id(),
            evicted: self.floor,
            pruned: self.pruned,
            eviction,
            pruning,
            // `None` and not a zeroed struct, on `eviction`'s and `pruning`'s
            // own semantics: a selection that collapsed nothing has nothing to
            // report, and a report of zero would put a `repeated` line in every
            // recording made under `Repeat::Always` saying the rule that is off
            // saved nothing.
            repeating: (repeated.spans > 0).then_some(repeated),
            diverged,
            superseded,
        }
    }

    /// The oldest turn that still fits, taking items newest first and never
    /// reaching past the floor. Returns an index into `turns`.
    ///
    /// Items, not turns: a folded task is kept or dropped whole, because half a
    /// summary is not a summary of half a task. So the floor only ever lands on
    /// an item boundary.
    ///
    /// Under [`Repeat::Once`] an item does not have a cost of its own: a turn
    /// added here becomes the **oldest** of the candidate window and takes
    /// ownership of its spans from whatever younger turn was rendering them, so
    /// what it adds is its own tokens less the copies it just made redundant.
    /// The two always come in pairs — a copy taken over is a copy this turn
    /// already carries — so the running total never goes down and the break is
    /// still a break.
    fn fits_from(
        &self,
        available: u32,
        counter: &dyn TokenCounter,
        repeat: Repeat,
        pruned: usize,
        results: Results,
        current: &Current,
    ) -> usize {
        let available = i64::from(available);
        let mut spent: i64 = 0;
        let mut start = self.turns.len();
        // A fragment in here is one some turn in the candidate window is
        // already rendering. Folded turns never enter it: a fold sends its
        // summary and not its turns, so a span inside one is not on the screen
        // and cannot stand in for anything.
        let mut rendered: HashSet<&Fragment> = HashSet::new();
        for item in self.items().iter().rev() {
            let mut tokens = i64::from(self.item_tokens(item, counter, pruned, results, current));
            if let (Repeat::Once, Item::Turn(index)) = (repeat, item) {
                // A pruned turn is not rendering its spans, so it neither owns
                // one nor makes a younger copy redundant: `item_tokens` has
                // already charged it for citations and the copies above it
                // stay.
                if *index >= pruned {
                    for fragment in &self.turns[*index].code_context {
                        if !rendered.insert(fragment) {
                            // What the copy would have cost *here*, which for a
                            // superseded span is its citation and not its
                            // bytes: the decision has to be made against the
                            // render it is deciding.
                            tokens -= i64::from(shown_fragment_tokens(fragment, current, counter));
                        }
                    }
                }
            }
            if spent + tokens > available {
                break;
            }
            spent += tokens;
            start = item.first();
        }
        start
    }

    /// What the window starting at `floor` costs, under the repeat rule in
    /// force.
    ///
    /// Forward, because ownership is oldest-first and a forward walk is where
    /// that is legible; [`Context::fits_from`] reads the same rule from the
    /// other end because it has to stop early. The two agree on the total and
    /// disagree about which turn is charged for a shared span, which is the
    /// difference between deciding and reporting.
    fn window_tokens(
        &self,
        floor: usize,
        counter: &dyn TokenCounter,
        repeat: Repeat,
        pruned: usize,
        results: Results,
        current: &Current,
    ) -> u32 {
        let mut total: i64 = 0;
        let mut shown: HashSet<&Fragment> = HashSet::new();
        for item in &self.items_from(floor) {
            let mut tokens = i64::from(self.item_tokens(item, counter, pruned, results, current));
            if let (Repeat::Once, Item::Turn(index)) = (repeat, item)
                && *index >= pruned
            {
                for fragment in &self.turns[*index].code_context {
                    if !shown.insert(fragment) {
                        tokens -= i64::from(shown_fragment_tokens(fragment, current, counter));
                    }
                }
            }
            total += tokens;
        }
        total.max(0) as u32
    }

    /// How far the prune line has to move to bring the window under `target`:
    /// the shallowest line that fits, or — where none does — the one that
    /// leaves the window smallest.
    ///
    /// Oldest first, one turn at a time, and the window recounted at each step
    /// rather than derived from what the turn carries. Deriving it would be
    /// wrong under [`Repeat::Once`]: the turn being pruned is the oldest one
    /// left, so it owns every span it carries, and what the window saves by
    /// pruning it is not the span but the span *less what a younger turn now
    /// has to render instead*.
    ///
    /// Which is why this takes a minimum rather than the first line it reaches.
    /// **Pruning is not monotone under rule A** — pruning the turn that owns a
    /// shared span hands the whole span to a younger turn and adds a citation
    /// where it stood, so the window can come out *bigger*. There the answer is
    /// to leave the line where it is: rule A has already taken that saving, and
    /// there is no second copy for rule B to find. See the 2026-09-08 addendum
    /// to `RECORD/2026-09-08.prune-behind.completed.md`.
    ///
    /// The loop runs on the call where the window overflowed and walks only the
    /// turns still in it.
    fn prune_to(
        &self,
        target: u32,
        counter: &dyn TokenCounter,
        repeat: Repeat,
        results: Results,
        current: &Current,
    ) -> usize {
        // Never below the floor: a turn nobody renders has nothing to give up,
        // and starting here is what keeps the line moving forward as the floor
        // does.
        let start = self.pruned.max(self.floor);
        let mut best = start;
        let mut smallest = self.window_tokens(self.floor, counter, repeat, start, results, current);

        let mut line = start;
        while smallest > target && line < self.turns.len() {
            line += 1;
            let cost = self.window_tokens(self.floor, counter, repeat, line, results, current);
            if cost < smallest {
                best = line;
                smallest = cost;
            }
            // The shallowest line that fits, and not the deepest that would:
            // the line is a ratchet, so what it gives up now it gives up for
            // the rest of the session.
            if cost <= target {
                return line;
            }
        }
        best
    }

    /// The stored count, unless it was produced by a different counter — in
    /// which case it is not a count of the same thing and gets redone.
    fn tokens_of(&self, turn: &Turn, counter: &dyn TokenCounter) -> u32 {
        match turn.counted_by == counter.id() {
            true => turn.tokens,
            false => {
                counter.count(&user_text(&turn.code_context, &turn.prompt))
                    + steps_tokens(&turn.steps, counter)
                    + counter.count(&turn.answer)
            }
        }
    }
}

/// The tool exchanges of a turn, counted as the messages they render as.
fn steps_tokens(steps: &[ToolStep], counter: &dyn TokenCounter) -> u32 {
    steps
        .iter()
        .map(|step| counter.count(&step.text) + counter.count(&step.result_text()))
        .sum()
}

/// The half of those exchanges a pruned turn gives up: what came back, and not
/// what was asked.
fn results_tokens(steps: &[ToolStep], counter: &dyn TokenCounter) -> u32 {
    steps
        .iter()
        .map(|step| counter.count(&step.result_text()))
        .sum()
}

/// The same half, as this policy will actually send it.
fn sent_results_tokens(steps: &[ToolStep], results: Results, counter: &dyn TokenCounter) -> u32 {
    steps
        .iter()
        .map(|step| counter.count(&result_under(step, results, counter)))
        .sum()
}

/// One tool result, as the line that stands in for it once its turn has been
/// pruned.
///
/// The tool that ran and what its answer was costing — enough for the model to
/// tell that the call was made and came back, so that making it again is a
/// decision rather than a discovery. The whole result is replaced, its `[name]
/// ok` head included, so the number cites the cost of exactly what is no longer
/// there; the assistant message that asked for it is untouched and says which
/// call this answers.
///
/// The same guard as a span's citation, for the same reason: `[read_file] ok\n42`
/// is cheaper than any line describing it, and a citation that is not cheaper is
/// not a saving. Keeping the result there is also what keeps the window's cost
/// monotone in the prune line.
/// One tool result as a policy sends it, behind the prune line.
///
/// **One function, because two would be a bug waiting.** The render builds the
/// messages and the accounting builds the number beside them, and this file's
/// own rule is that `buckets` describes `messages` rather than estimating it.
/// Under two values of [`Results`] that was one `match` each in two places;
/// under three it is a per-tool question, and a second copy of it would be a
/// window whose reported cost and actual cost drift apart by exactly one
/// tool's output.
fn result_under(step: &ToolStep, results: Results, counter: &dyn TokenCounter) -> String {
    match results {
        Results::Kept => step.result_text(),
        Results::Cited => pruned_result_text(step, counter),
        Results::CitedReads => match could_be_read_again(&step.call.name) {
            true => pruned_result_text(step, counter),
            false => step.result_text(),
        },
    }
}

fn pruned_result_text(step: &ToolStep, counter: &dyn TokenCounter) -> String {
    let full = step.result_text();
    let citation = format!(
        "[{}] output — {} tokens, pruned",
        step.call.name,
        counter.count(&full)
    );
    match counter.count(&citation) < counter.count(&full) {
        true => citation,
        false => full,
    }
}

/// One fragment, as it is rendered inside a user message.
fn fragment_text(fragment: &Fragment) -> String {
    format!("// {}\n{}\n\n", fragment.path, fragment.text)
}

/// What one fragment costs where it is rendered.
fn fragment_tokens(fragment: &Fragment, counter: &dyn TokenCounter) -> u32 {
    counter.count(&fragment_text(fragment))
}

fn fragments_tokens(fragments: &[&Fragment], counter: &dyn TokenCounter) -> u32 {
    fragments
        .iter()
        .map(|fragment| fragment_tokens(fragment, counter))
        .sum()
}

/// One fragment, as the line that stands in for bytes a later turn has read
/// differently.
///
/// Shaped like [`pruned_fragment_text`] and deliberately **unguarded**, which
/// is the one place the two part company. A pruned span's citation is a
/// *saving*, so it is not taken where it would cost more than the span it
/// replaces — a citation that is not cheaper is not a saving. This one is a
/// *correction*: what it replaces is not expensive, it is untrue, and a
/// two-line fragment being cheap to send is not a reason to send a body the
/// session knows is stale. The window pays a few tokens for it and the
/// `history` bucket says so.
fn superseded_fragment_text(fragment: &Fragment, counter: &dyn TokenCounter) -> String {
    format!(
        "// {} — {} tokens, superseded\n\n",
        fragment.path,
        fragment_tokens(fragment, counter)
    )
}

fn superseded_fragment_tokens(fragment: &Fragment, counter: &dyn TokenCounter) -> u32 {
    counter.count(&superseded_fragment_text(fragment, counter))
}

/// One fragment, as this render actually sends it: its bytes, or the line that
/// cites them where a later turn has read the same span differently.
///
/// One function, for [`result_under`]'s reason: the render builds the messages
/// and the accounting builds the number beside them, and two copies of this
/// `match` would be a window whose reported cost and actual cost drift apart by
/// exactly one superseded span.
fn shown_fragment_text(
    fragment: &Fragment,
    current: &Current,
    counter: &dyn TokenCounter,
) -> String {
    match current.superseded(fragment) {
        true => superseded_fragment_text(fragment, counter),
        false => fragment_text(fragment),
    }
}

fn shown_fragment_tokens(
    fragment: &Fragment,
    current: &Current,
    counter: &dyn TokenCounter,
) -> u32 {
    counter.count(&shown_fragment_text(fragment, current, counter))
}

fn shown_fragments_tokens(
    fragments: &[&Fragment],
    current: &Current,
    counter: &dyn TokenCounter,
) -> u32 {
    fragments
        .iter()
        .map(|fragment| shown_fragment_tokens(fragment, current, counter))
        .sum()
}

/// One fragment, as the line that stands in for it once its turn has been
/// pruned.
///
/// The path, the span and what it was costing — enough for the model to tell
/// that the file was read, so that reading it again is a decision rather than a
/// discovery, and not enough to be prose. A summary would be model text in a
/// region every later turn is built on, which is the shape the fold already
/// rejects.
///
/// Where the citation would cost more than the span it replaces — a two-line
/// fragment, say — the span stays: a citation that is not cheaper is not a
/// saving, and pretending otherwise would make the window's cost stop being
/// monotone in the prune line.
fn pruned_fragment_text(fragment: &Fragment, counter: &dyn TokenCounter) -> String {
    let citation = format!(
        "// {} — {} tokens, pruned\n\n",
        fragment.path,
        fragment_tokens(fragment, counter)
    );
    match counter.count(&citation) < fragment_tokens(fragment, counter) {
        true => citation,
        false => fragment_text(fragment),
    }
}

/// A span that a resume could not read again, as the line that stands in for it.
///
/// A [`Fragment`] like any other, so the renderer, the dedup and the accounting
/// need no special case — and a comment rather than prose, for the reason a
/// pruned span's citation is one: the model is told the file was read and that
/// it is not readable from here, and nothing in the region every later turn is
/// built on is model text.
///
/// It carries the reason rather than only the fact, because *the policy refuses
/// this path* and *this file is gone* mean opposite things about what to do
/// next, and a reader who cannot tell them apart will guess.
fn unreadable_fragment(spec: &str, error: &crate::fragment::LoadError) -> Fragment {
    Fragment {
        path: spec.to_string(),
        text: format!("// not readable when this session was resumed: {error}"),
    }
}

/// What one fragment costs where its turn has been pruned.
fn pruned_fragment_tokens(fragment: &Fragment, counter: &dyn TokenCounter) -> u32 {
    counter.count(&pruned_fragment_text(fragment, counter))
}

/// The user half of a pruned turn: its citations, then what was asked.
fn pruned_user_text(code_context: &[Fragment], prompt: &str, counter: &dyn TokenCounter) -> String {
    let mut text: String = code_context
        .iter()
        .map(|fragment| pruned_fragment_text(fragment, counter))
        .collect();
    text.push_str(prompt);
    text
}

/// What the session knows each path's bytes to be: the body carried by the
/// **newest** turn that read it, with the turn being asked ahead of all of them.
///
/// Over every turn the session holds, the evicted ones included, and that is
/// the ratchet rather than a field that remembers. `turns` only ever grows by
/// appending a younger turn, so the newest turn carrying a path only ever moves
/// forward — which means a body this says is stale cannot become current again
/// by anything *leaving* the window. The floor and the prune line both needed a
/// stored ratchet to get that property; here it falls out of the derivation,
/// which is `SessionView::counts()`'s own argument one structure along: a fact
/// derived from what the session already holds cannot drift from it.
///
/// The one thing that *does* move it back is a file edited back to what it was,
/// which a later turn then reads. That is not the ratchet failing — the body it
/// names is current, because somebody read it and it was there.
///
/// Empty under [`Repeat::Always`], which is what keeps supersession rule A's
/// repair rather than a fourth window rule: see [`Selection::superseded`].
///
/// The current body per path is cloned rather than borrowed, and it is the one
/// allocation this costs. [`Context::select`] takes `&mut self` and moves the
/// floor and the prune line between building this and rendering with it, so a
/// borrow of `turns` cannot live across the decision; a handful of distinct
/// paths cloned once per render is the cheaper half of that trade, and the
/// render already clones every message it builds.
struct Current {
    /// One entry per path, in the order the session first carried it.
    ///
    /// A `Vec` and a linear scan, for [`diverged_paths`]' reason: a window
    /// holds tens of fragments over a handful of paths, and a `HashMap` would
    /// spend an allocation per path to save comparisons nobody can measure.
    bodies: Vec<(String, String)>,
}

impl Current {
    fn of(turns: &[Turn], asking: &[Fragment], repeat: Repeat) -> Self {
        let mut bodies: Vec<(String, String)> = Vec::new();
        if matches!(repeat, Repeat::Always) {
            return Self { bodies };
        }
        // Oldest first and last-wins, so the body that survives the walk is the
        // one the newest reader of that path got. The turn being asked comes
        // last because it is the youngest there is: its spans were loaded
        // through the sandbox in this very call.
        let carried = turns
            .iter()
            .flat_map(|turn| turn.code_context.iter())
            .chain(asking);
        for fragment in carried {
            match bodies.iter_mut().find(|(path, _)| *path == fragment.path) {
                Some(one) => one.1.clone_from(&fragment.text),
                None => bodies.push((fragment.path.clone(), fragment.text.clone())),
            }
        }
        Self { bodies }
    }

    /// Whether this fragment's bytes are ones a later turn has read differently.
    fn superseded(&self, fragment: &Fragment) -> bool {
        self.bodies
            .iter()
            .any(|(path, body)| *path == fragment.path && *body != fragment.text)
    }
}

/// What this turn renders, and what a turn already rendered in this same call
/// is showing for it.
///
/// `shown` carries across the whole render, which is the invariant the rule
/// rests on: a span is only ever skipped because something *else in this
/// prompt* has it. Under [`Repeat::Always`] nothing is recorded and nothing is
/// skipped, so a run made without the rule cannot be quietly changed by it.
fn split_shown<'a>(
    code_context: &'a [Fragment],
    shown: &mut HashSet<&'a Fragment>,
    repeat: Repeat,
) -> (Vec<&'a Fragment>, Vec<&'a Fragment>) {
    match repeat {
        Repeat::Always => (code_context.iter().collect(), Vec::new()),
        Repeat::Once => code_context
            .iter()
            .partition(|fragment| shown.insert(fragment)),
    }
}

/// One replaced span, folded into the report for its path.
///
/// One entry per path and one `turns` entry per turn, however many of that
/// turn's spans moved: the report answers *where do I look*, and the spans and
/// tokens beside it answer *how much*.
fn note_superseded(
    into: &mut Vec<Superseded>,
    fragment: &Fragment,
    turn: TurnId,
    counter: &dyn TokenCounter,
) {
    let tokens = fragment_tokens(fragment, counter);
    match into.iter_mut().find(|one| one.path == fragment.path) {
        Some(one) => {
            if one.turns.last() != Some(&turn) {
                one.turns.push(turn);
            }
            one.spans += 1;
            one.tokens += tokens;
        }
        None => into.push(Superseded {
            path: fragment.path.clone(),
            turns: vec![turn],
            spans: 1,
            tokens,
            counter: counter.id(),
        }),
    }
}

/// Every path this render sent under more than one body, in the order the
/// window first carried each one.
///
/// Over what was **sent**, not over what is stored. A turn below the prune line
/// has given its span up and an evicted turn is gone, so a detector reading
/// `code_context` would report contradictions the model was never shown — which
/// is the same mistake `Pruned::tokens` documents one field along, where the
/// saving is the difference of two windows rather than the sum of the spans.
///
/// `Vec` and a linear scan rather than a map: a window holds tens of fragments,
/// the order is part of the answer, and a `HashMap` here would cost an
/// allocation per path to save comparisons nobody can measure.
fn diverged_paths(sent: &[(&str, &str, Option<TurnId>)]) -> Vec<Diverged> {
    /// One path's tally while the scan runs. Named fields rather than a tuple
    /// because three of the four are collections and `.1` against `.2` at a
    /// call site is the kind of thing that reads fine when written.
    struct Carried<'a> {
        path: &'a str,
        bodies: Vec<&'a str>,
        turns: Vec<TurnId>,
        asking: bool,
    }

    // One entry per path, in the order the window first carried it. Built in
    // full and filtered after, because deciding as we go would have to go back
    // for the carriers of the bodies that came before the disagreement did.
    let mut carried: Vec<Carried> = Vec::new();
    for (path, text, turn) in sent {
        let entry = match carried.iter().position(|one| one.path == *path) {
            Some(at) => &mut carried[at],
            None => {
                carried.push(Carried {
                    path,
                    bodies: Vec::new(),
                    turns: Vec::new(),
                    asking: false,
                });
                carried.last_mut().expect("just pushed")
            }
        };
        if !entry.bodies.contains(text) {
            entry.bodies.push(text);
        }
        match turn {
            Some(id) => entry.turns.push(*id),
            None => entry.asking = true,
        }
    }

    carried
        .into_iter()
        .filter(|one| one.bodies.len() > 1)
        .map(|one| Diverged {
            path: one.path.to_string(),
            turns: one.turns,
            asking: one.asking,
            bodies: one.bodies.len(),
        })
        .collect()
}

/// The fragments alone, as they are rendered inside a user message.
fn fragments_text<'a>(code_context: impl IntoIterator<Item = &'a Fragment>) -> String {
    code_context.into_iter().map(fragment_text).collect()
}

/// The user half of a turn: its code, then what was asked.
///
/// Fused into one message rather than sent as two, because a standalone
/// context message leaves two `user` messages back to back and chat templates
/// disagree about what that means.
fn user_text<'a>(code_context: impl IntoIterator<Item = &'a Fragment>, prompt: &str) -> String {
    let mut text = fragments_text(code_context);
    text.push_str(prompt);
    text
}

/// The same, with each span rendered as this call will send it.
///
/// [`user_text`] stays beside it and is not a special case of it: the stored
/// count of a turn is what that turn's own text weighs, and it is taken once
/// when the turn closes, before there is any later turn to supersede anything.
fn shown_user_text<'a>(
    code_context: impl IntoIterator<Item = &'a Fragment>,
    prompt: &str,
    current: &Current,
    counter: &dyn TokenCounter,
) -> String {
    let mut text: String = code_context
        .into_iter()
        .map(|fragment| shown_fragment_text(fragment, current, counter))
        .collect();
    text.push_str(prompt);
    text
}

/// The two-call lifecycle the tests below were written against, in one call.
///
/// Test-only, and deliberately not on the public surface: approving *is*
/// closing, so a caller that wants an approved job wants
/// [`Context::approve_plan`] and everything it does on the way. This exists so
/// that a test about eviction or selection does not have to restate the gate.
#[cfg(test)]
impl Context {
    fn approved_job(&mut self, objective: impl Into<String>, plan: Plan) -> JobId {
        self.approve_plan(
            objective,
            plan.clone(),
            plan,
            crate::job::PlanSource::Written,
            ApprovedBy::Operator,
            &ApproximateCounter,
        )
        .job
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::backend::Role;
    use crate::sandbox::{Applied, Verdict};
    use crate::tools::{ToolCall, ToolOutcome};

    /// One token per word, and it says how often it was asked. The second part
    /// is what makes "counted once" testable at all.
    #[derive(Default)]
    struct WordCounter {
        calls: AtomicUsize,
    }

    impl TokenCounter for WordCounter {
        fn id(&self) -> Counter {
            Counter::Model { id: "words".into() }
        }

        fn count(&self, text: &str) -> u32 {
            self.calls.fetch_add(1, Ordering::Relaxed);
            text.split_whitespace().count() as u32
        }
    }

    fn context_with(turns: usize, counter: &dyn TokenCounter) -> Context {
        let mut context = Context::new("system prompt here");
        for n in 0..turns {
            context.push_turn(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                vec![],
                counter,
            );
        }
        context
    }

    #[test]
    fn an_unknown_window_selects_nothing_and_evicts_nothing() {
        let counter = WordCounter::default();
        let mut context = context_with(5, &counter);

        let selection = context.select(
            "now this",
            &[],
            Budget::new(0, 512, Eviction::Turn),
            &counter,
        );

        assert_eq!(selection.limit, None);
        assert_eq!(selection.evicted, 0);
        assert_eq!(selection.messages.len(), 5 * 2 + 2);
        // No window, no headroom to plot.
        assert!(!selection.buckets.iter().any(|b| b.name == "reserve"));
    }

    #[test]
    fn eviction_drops_whole_turns_and_never_orphans_an_answer() {
        let counter = WordCounter::default();
        let mut context = context_with(6, &counter);

        // Room for the fixed part and two turns, no more.
        let selection = context.select(
            "now this",
            &[],
            Budget::new(30, 4, Eviction::Turn),
            &counter,
        );

        assert!(selection.evicted > 0, "the point of the case");
        let roles: Vec<Role> = selection.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles[0], Role::System);
        // Everything between the system block and the current turn alternates,
        // starting on a user message.
        for (index, role) in roles[1..].iter().enumerate() {
            let expected = match index % 2 {
                0 => Role::User,
                _ => Role::Assistant,
            };
            assert_eq!(*role, expected, "message {} of {roles:?}", index + 1);
        }
        assert_eq!(roles.last(), Some(&Role::User));
    }

    #[test]
    fn what_is_kept_is_the_newest_turns() {
        let counter = WordCounter::default();
        let mut context = context_with(6, &counter);

        let selection = context.select(
            "now this",
            &[],
            Budget::new(30, 4, Eviction::Turn),
            &counter,
        );

        let kept: Vec<&String> = selection
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| &m.content)
            .collect();
        assert!(!kept.iter().any(|text| text.contains("number 0")));
        assert!(kept.iter().any(|text| text.contains("number 5")));
    }

    #[test]
    fn the_system_block_is_byte_identical_whatever_the_history() {
        let counter = WordCounter::default();
        let mut empty = Context::new("system prompt here");
        let mut full = context_with(4, &counter);

        let first = empty.select(
            "hola",
            &[],
            Budget::new(8192, 512, Eviction::Turn),
            &counter,
        );
        let later = full.select(
            "hola",
            &[],
            Budget::new(8192, 512, Eviction::Turn),
            &counter,
        );

        assert_eq!(first.messages[0].content, later.messages[0].content);
        assert_eq!(first.messages[0].role, Role::System);
    }

    #[test]
    fn code_is_fused_into_the_current_user_message_never_the_system_block() {
        let counter = WordCounter::default();
        let mut context = context_with(2, &counter);
        let code = vec![Fragment {
            path: "src/lib.rs".into(),
            text: "fn main() {}".into(),
        }];

        let selection = context.select(
            "explain this",
            &code,
            Budget::new(8192, 512, Eviction::Turn),
            &counter,
        );

        assert!(!selection.messages[0].content.contains("src/lib.rs"));
        let last = selection.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(last.content.contains("src/lib.rs"));
        assert!(last.content.ends_with("explain this"));
    }

    #[test]
    fn a_closed_turn_is_counted_once_not_once_per_later_turn() {
        let counter = WordCounter::default();
        let mut context = context_with(10, &counter);
        let after_building = counter.calls.load(Ordering::Relaxed);

        context.select(
            "now this",
            &[],
            Budget::new(8192, 512, Eviction::Turn),
            &counter,
        );

        // System, the tool definitions, the map, the fragments, the prompt.
        // The ten stored turns are not re-counted, which is the whole point of
        // storing their counts.
        assert_eq!(counter.calls.load(Ordering::Relaxed) - after_building, 5);
    }

    #[test]
    fn a_turn_counted_by_another_counter_is_recounted_rather_than_summed() {
        let words = WordCounter::default();
        let mut context = Context::new("system prompt here");
        context.push_turn(1, "one two three", "four five", vec![], &ApproximateCounter);

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &words,
        );

        // The stored count came from chars/4; the bar is in words, so the turn
        // is re-counted rather than added in a foreign unit.
        let history = selection
            .buckets
            .iter()
            .find(|b| b.name == "history")
            .unwrap();
        assert_eq!(history.tokens, 5);
    }

    #[test]
    fn the_current_turn_survives_a_window_too_small_for_it() {
        let counter = WordCounter::default();
        let mut context = context_with(3, &counter);

        let selection = context.select(
            "a very long question indeed",
            &[],
            Budget::new(2, 8, Eviction::Turn),
            &counter,
        );

        assert_eq!(selection.evicted, 3);
        assert_eq!(selection.messages.len(), 2, "system and the prompt");
        assert_eq!(selection.messages[1].content, "a very long question indeed");
    }

    /// Under the native transport the tools are counted — the specs' JSON is
    /// what the request sends — and never rendered: the server's template does
    /// that. See `RECORD/2026-09-30.native-tool-calls.completed.md`.
    #[test]
    fn native_tools_are_counted_and_not_rendered() {
        let counter = WordCounter::default();
        let budget = Budget::new(8192, 512, Eviction::Turn);
        let fenced = Context::new("system prompt here")
            .with_tools("# Tools\nread_file")
            .with_map("# Repository map\nsrc/lib.rs")
            .select("one", &[], budget, &counter);
        let native = Context::new("system prompt here")
            .with_native_tools(r#"[{"type":"function","function":{"name":"read_file"}}]"#)
            .with_map("# Repository map\nsrc/lib.rs")
            .select("one", &[], budget, &counter);

        assert!(fenced.messages[0].content.contains("# Tools"));
        assert!(!native.messages[0].content.contains("read_file"));
        // What the trace carries beside the prompt, so a reader can see what
        // the tools bucket counted.
        assert_eq!(
            Context::new("s")
                .with_native_tools(r#"[{"name":"read_file"}]"#)
                .native_tools(),
            Some(r#"[{"name":"read_file"}]"#)
        );
        assert_eq!(Context::new("s").with_tools("# Tools").native_tools(), None);
        assert_eq!(Context::new("s").with_native_tools("").native_tools(), None);
        assert_eq!(
            native.messages[0].content, "system prompt here\n\n# Repository map\nsrc/lib.rs",
            "the map still follows the system text"
        );
        let tools = |selection: &Selection| {
            selection
                .buckets
                .iter()
                .find(|bucket| bucket.name == "tools")
                .expect("the tools bucket")
                .tokens
        };
        assert!(tools(&native) > 0, "what is sent is counted");
    }

    #[test]
    fn the_map_is_prefix_and_stays_byte_identical_as_the_history_grows() {
        // The placement argument is worthless if the map wobbles: it is above
        // the history precisely so that a prompt cache can keep it.
        let counter = WordCounter::default();
        let mut context = Context::new("system prompt here")
            .with_tools("# Tools\nread_file")
            .with_map("# Repository map\nsrc/lib.rs\n     1  pub fn main");
        let budget = Budget::new(8192, 512, Eviction::Turn);

        let first = context.select("one", &[], budget, &counter);
        context.push_turn(1, "one", "an answer", vec![], &counter);
        let second = context.select("two", &[], budget, &counter);

        assert_eq!(
            first.messages[0].content, second.messages[0].content,
            "the whole prefix is one message and it does not move",
        );
        assert!(
            first.messages[0].content.ends_with("     1  pub fn main"),
            "system, then tools, then the map — most stable first: {}",
            first.messages[0].content,
        );
        let map = |selection: &Selection| {
            selection
                .buckets
                .iter()
                .find(|bucket| bucket.name == "map")
                .expect("the map bucket")
                .tokens
        };
        assert!(map(&first) > 0);
        assert_eq!(
            map(&first),
            map(&second),
            "and it costs the same every turn"
        );
    }

    #[test]
    fn a_map_is_counted_against_the_window_rather_than_added_beside_it() {
        // It is in the prompt, so it is in the budget: history has to give way
        // for it like anything else. A map that was free would be a map that
        // silently ate the answer.
        let counter = WordCounter::default();
        let mut bare = context_of_equal_turns(6, &counter);
        let mut mapped = context_of_equal_turns(6, &counter);
        mapped = mapped.with_map("aa bb cc dd ee ff gg hh ii jj kk ll mm nn oo pp");
        let system = counter.count(bare.system());
        let budget = Budget::new(system + 1 + 8 * 5, 0, Eviction::Turn);

        let without = bare.select("q", &[], budget, &counter);
        let with = mapped.select("q", &[], budget, &counter);

        assert!(
            with.evicted > without.evicted,
            "the map cost history: {} evicted with it, {} without",
            with.evicted,
            without.evicted,
        );
    }

    #[test]
    fn buckets_are_in_prompt_order_with_the_reserve_last() {
        let counter = WordCounter::default();
        let mut context = context_with(1, &counter);

        let selection = context.select(
            "hola",
            &[],
            Budget::new(8192, 512, Eviction::Turn),
            &counter,
        );

        let names: Vec<&str> = selection.buckets.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "system",
                "tools",
                "map",
                "summaries",
                "history",
                "code",
                "prompt",
                "reserve"
            ]
        );
    }

    /// A task with turns in it, closed or not, for the fold cases below.
    fn context_with_closed_task(live: usize, counter: &dyn TokenCounter) -> (Context, TaskId) {
        let mut context = Context::new("system prompt here");
        let task = context.approved_job("explain the context manager", Plan::default());
        for n in 0..3 {
            context.push_turn(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                vec![],
                counter,
            );
        }
        context.close_task(task, counter);
        for n in 0..live {
            context.push_turn(
                n as TurnId + 4,
                format!("later question {n} padded out"),
                format!("later answer {n} padded out"),
                vec![],
                counter,
            );
        }
        (context, task)
    }

    /// The fold's own claim: which turns stopped being sent, and what they were
    /// worth in the prompt they left. Counted at the close because it is only
    /// true then — see
    /// `RECORD/2026-09-08.what-a-fold-writes-down.completed.md`.
    #[test]
    fn a_fold_writes_down_what_it_replaced() {
        let counter = WordCounter::default();
        let (context, task) = context_with_closed_task(0, &counter);

        let replaced = context.replaced_by(task).expect("the close counted it");
        assert_eq!(
            replaced.turns,
            vec![1, 2, 3],
            "named rather than counted: which turns stopped being sent",
        );

        let summary = context
            .job(task)
            .and_then(|job| job.summary.as_ref())
            .expect("a closed job has one");
        assert_eq!(
            summary.replaced.as_ref(),
            Some(&replaced),
            "the numbers live with the summary they are subtracted from",
        );
        assert_eq!(
            replaced.summary_tokens, summary.tokens,
            "the fold carries the summary's own count, so a reader never counts it again",
        );
        assert!(
            replaced.tokens > summary.tokens,
            "three turns cost more than the line that stands in for them: \
             {} replaced by {}",
            replaced.tokens,
            summary.tokens,
        );

        // The same unit on both sides, which is the whole reason the count
        // happens at the close: `tokens_of` is what the `history` bucket sums.
        let by_hand: u32 = context
            .turns
            .iter()
            .filter(|turn| turn.job == task)
            .map(|turn| context.tokens_of(turn, &counter))
            .sum();
        assert_eq!(replaced.tokens, by_hand);
    }

    /// A reopened job is being written again, so nothing has been saved and the
    /// numbers go where the summary goes.
    #[test]
    fn reopening_drops_what_the_fold_wrote_down() {
        let counter = WordCounter::default();
        let (mut context, task) = context_with_closed_task(0, &counter);
        assert!(context.replaced_by(task).is_some());

        context.reopen_task(task);

        assert!(context.replaced_by(task).is_none());
    }

    #[test]
    fn the_fold_keeps_what_the_task_was_shown() {
        // The probe's turns 17 and 18: a task grounded by a fragment, closed,
        // and asked about afterwards. Before this, the summary said "no tools
        // ran" and the file was gone. See
        // `RECORD/2026-08-30.the-fold-probe-run.completed.md`.
        let counter = WordCounter::default();
        let mut context = Context::new("system prompt here");
        let task = context.approved_job("work out what the policy grants", Plan::default());
        context.push_turn(
            1,
            "which programs does this policy allow?",
            "cargo, rustc, git, rg, ls",
            vec![Fragment {
                path: "luu.toml:1-3".into(),
                text: "[sandbox]\ncommands = [\"cargo\", \"rg\"]\n".into(),
            }],
            &counter,
        );
        let summary = context.close_task(task, &counter).unwrap();

        assert!(summary.contains("luu.toml:1-3"), "{summary}");
        assert!(
            summary.contains("commands = [\"cargo\", \"rg\"]"),
            "the fragment was in hand at the close: {summary}",
        );

        let selection = context.select(
            "which programs does the policy allow?",
            &[],
            Budget::new(0, 0, Eviction::Turn),
            &counter,
        );
        assert!(
            selection.messages[2].content.contains("cargo"),
            "what the folded task read is still in the prompt: {:?}",
            selection.messages[2].content,
        );
    }

    #[test]
    fn a_closed_task_folds_its_turns_into_one_exchange() {
        let counter = WordCounter::default();
        let (mut context, _) = context_with_closed_task(0, &counter);

        let selection =
            context.select("now this", &[], Budget::new(0, 0, Eviction::Turn), &counter);

        assert_eq!(
            selection.messages.len(),
            1 + 2 + 1,
            "system, the folded pair, and the current prompt",
        );
        assert_eq!(selection.messages[1].role, Role::User);
        assert_eq!(
            selection.messages[1].content, "explain the context manager",
            "the user half of a fold is the objective as approved, not a sentence about it",
        );
        assert!(selection.messages[2].content.contains("[job closed]"));
        assert_eq!(
            context.turns().len(),
            3,
            "closing is an event, not a mutation: the turns are still there",
        );

        let bucket = |name: &str| {
            selection
                .buckets
                .iter()
                .find(|b| b.name == name)
                .unwrap()
                .tokens
        };
        assert!(bucket("summaries") > 0);
        assert_eq!(bucket("history"), 0, "nothing is left unfolded");
    }

    #[test]
    fn reopening_sends_the_turns_verbatim_again() {
        let counter = WordCounter::default();
        let (mut context, task) = context_with_closed_task(0, &counter);
        let folded = context.select("now this", &[], Budget::new(0, 0, Eviction::Turn), &counter);

        assert!(context.reopen_task(task));
        let reopened = context.select("now this", &[], Budget::new(0, 0, Eviction::Turn), &counter);

        assert_eq!(folded.messages.len(), 4);
        assert_eq!(
            reopened.messages.len(),
            3 * 2 + 2,
            "the fold stopped applying; nothing had to be recovered",
        );
    }

    #[test]
    fn a_folded_task_is_kept_or_dropped_whole_at_every_window_size() {
        // The fold and eviction meet here: a folded block is one item, so the
        // floor can land before it or after it and never inside it.
        for limit in (20..240).step_by(4) {
            let counter = WordCounter::default();
            let (mut context, _) = context_with_closed_task(3, &counter);
            let selection = context.select(
                "now this",
                &[],
                Budget::new(limit, 0, Eviction::Turn),
                &counter,
            );
            assert!(
                selection.evicted == 0 || selection.evicted >= 3,
                "limit {limit} cut inside the folded task ({} turns dropped): \
                 half a summary is not a summary of half a task",
                selection.evicted,
            );
        }
    }

    /// Every turn belongs to a job, from the first one. What this test used to
    /// assert was that the turns before an approval belonged to nothing; the
    /// draft they open is what holds them now, and the approval between them is
    /// where the session changes jobs rather than acquires one.
    #[test]
    fn turns_are_attributed_to_the_job_that_was_open_when_they_were_pushed() {
        let counter = WordCounter::default();
        let mut context = Context::new("system");
        context.push_turn(1, "before any plan", "a", vec![], &counter);
        context.push_turn(2, "still drafting", "b", vec![], &counter);
        let job = context.approved_job("do the thing", Plan::default());
        context.push_turn(3, "inside the job", "c", vec![], &counter);

        let attributed: Vec<JobId> = context.turns().iter().map(|turn| turn.job).collect();
        assert_eq!(
            attributed,
            vec![1, 1, job],
            "the drafting turns share the draft they opened, and the last one is work",
        );
        assert_eq!(job, 2, "the draft took id 1, and the approval handed out 2");
        assert!(
            context.job(1).unwrap().is_draft(),
            "a draft is a job whose plan nobody has written",
        );
    }

    #[test]
    fn the_approximate_counter_says_so() {
        let mut context = Context::new("system");
        let selection = context.select(
            "hola",
            &[],
            Budget::new(8192, 512, Eviction::Turn),
            &ApproximateCounter,
        );
        assert!(selection.counter.is_approximate());
    }

    /// Turns of a known size, so a budget can be written in turns rather than
    /// in tokens nobody can check by eye.
    fn context_of_equal_turns(turns: usize, counter: &dyn TokenCounter) -> Context {
        let mut context = Context::new("sys");
        for n in 0..turns {
            // Four words each way: every turn costs exactly eight.
            context.push_turn(
                n as TurnId + 1,
                format!("q {n} aa bb"),
                format!("a {n} cc dd"),
                vec![],
                counter,
            );
        }
        context
    }

    #[test]
    fn a_selection_that_cuts_says_which_turns_it_dropped() {
        // Eight tokens a turn, so the arithmetic is checkable by eye: room for
        // three turns and a fourth one asked.
        let counter = WordCounter::default();
        let mut context = context_of_equal_turns(4, &counter);
        let system = counter.count(context.system());

        let selection = context.select(
            "q",
            &[],
            Budget::new(system + 1 + 8 * 3, 0, Eviction::Turn),
            &counter,
        );

        let evicted = selection.eviction.expect("the window could not hold four");
        assert_eq!(
            evicted.turns,
            vec![1],
            "the per-turn policy drops the minimum, and names it",
        );
        assert_eq!(evicted.tokens, 8, "what the dropped turn was worth");
        assert_eq!(evicted.counter, counter.id(), "a count says who counted it");
        assert_eq!(evicted.policy, Eviction::Turn);
    }

    #[test]
    fn a_selection_that_fits_drops_nothing_and_says_so() {
        let counter = WordCounter::default();
        let mut context = context_of_equal_turns(3, &counter);
        let system = counter.count(context.system());

        let selection = context.select(
            "q",
            &[],
            Budget::new(system + 1 + 8 * 4, 0, Eviction::Turn),
            &counter,
        );

        assert!(
            selection.eviction.is_none(),
            "a tombstone for a cut that did not happen is a lie a panel would plot",
        );
    }

    #[test]
    fn the_cut_names_session_turns_rather_than_positions_in_the_history() {
        // A turn that produced nothing is never pushed — both call sites skip
        // it — so the third turn of a session can be the second one here. An
        // index would name the wrong turn, quietly, in the one file whose
        // purpose is to be trusted months later.
        let counter = WordCounter::default();
        let mut context = Context::new("sys");
        context.push_turn(1, "q 1 aa bb", "a 1 cc dd", vec![], &counter);
        // 2 is missing: it was cancelled before it produced anything.
        context.push_turn(3, "q 3 aa bb", "a 3 cc dd", vec![], &counter);
        context.push_turn(4, "q 4 aa bb", "a 4 cc dd", vec![], &counter);
        let system = counter.count(context.system());

        let selection = context.select(
            "q",
            &[],
            Budget::new(system + 1 + 8 * 2, 0, Eviction::Turn),
            &counter,
        );

        assert_eq!(
            selection.eviction.expect("a cut").turns,
            vec![1],
            "the number the session handed out, not the index",
        );
    }

    #[test]
    fn the_block_policy_reports_the_deeper_cut_it_made() {
        let counter = WordCounter::default();
        let mut context = context_of_equal_turns(6, &counter);
        let system = counter.count(context.system());
        let policy = Eviction::Block { low_water: 0.5 };

        let selection = context.select(
            "q",
            &[],
            Budget::new(system + 1 + 8 * 5, 0, policy),
            &counter,
        );

        let evicted = selection.eviction.expect("the window could not hold six");
        assert_eq!(
            evicted.policy, policy,
            "which cut this was, not just that one happened"
        );
        assert!(
            evicted.turns.len() > 1,
            "block eviction cuts past what it needs: {:?}",
            evicted.turns,
        );
        assert_eq!(
            evicted.tokens as usize,
            evicted.turns.len() * 8,
            "the tokens are the turns that left, not a share of them",
        );
    }

    #[test]
    fn a_folded_task_leaving_the_window_names_the_turns_it_covered() {
        // The two ways history stops being sent, composed: the task is already
        // folded to its summary when the summary itself falls out.
        let counter = WordCounter::default();
        let (mut context, _) = context_with_closed_task(3, &counter);

        let mut cut = None;
        // Tightened one token at a time, so the first cut is caught wherever
        // the summary happens to land rather than at a limit picked by hand.
        for limit in (1..=200).rev() {
            let selection =
                context.select("q", &[], Budget::new(limit, 0, Eviction::Turn), &counter);
            if let Some(evicted) = selection.eviction {
                cut = Some(evicted);
                break;
            }
        }

        let evicted = cut.expect("some limit is too small for the whole history");
        assert_eq!(
            evicted.turns,
            vec![1, 2, 3],
            "a folded task is kept or dropped whole, and its turns are named",
        );
    }

    #[test]
    fn a_turn_that_left_the_window_does_not_come_back_when_room_reappears() {
        let counter = WordCounter::default();
        let mut context = context_of_equal_turns(6, &counter);
        let budget = Budget::new(60, 0, Eviction::Turn);

        // A prompt big enough to force eviction, then a tiny one that would
        // leave room for what was just dropped.
        let long: String = std::iter::repeat_n("pad", 30).collect::<Vec<_>>().join(" ");
        let evicted = context.select(&long, &[], budget, &counter).evicted;
        assert!(
            evicted > 0,
            "the case only exists once something was dropped"
        );

        let after = context.select("hi", &[], budget, &counter);
        assert_eq!(
            after.evicted, evicted,
            "resurrecting a turn rewrites the history from its front, which is \
             the one thing the prefix cache cannot survive",
        );
    }

    #[test]
    fn block_eviction_cuts_deeper_and_then_holds_still() {
        let counter = WordCounter::default();
        // Eight turns of eight tokens: 64 of history against a window of 40.
        let mut per_turn = context_of_equal_turns(8, &counter);
        let mut block = context_of_equal_turns(8, &counter);

        let window = 40;
        let turn = Budget::new(window, 0, Eviction::Turn);
        let blocks = Budget::new(window, 0, Eviction::Block { low_water: 0.5 });

        let first_per_turn = per_turn.select("q", &[], turn, &counter).evicted;
        let first_block = block.select("q", &[], blocks, &counter).evicted;
        assert!(
            first_block > first_per_turn,
            "the block policy pays for its stability up front: {first_block} vs {first_per_turn}",
        );

        // A closed turn is appended to each, and only the per-turn policy has
        // to move its front again to make room.
        per_turn.push_turn(9, "q 8 aa bb", "a 8 cc dd", vec![], &counter);
        block.push_turn(9, "q 8 aa bb", "a 8 cc dd", vec![], &counter);

        let next_per_turn = per_turn.select("q", &[], turn, &counter).evicted;
        let next_block = block.select("q", &[], blocks, &counter).evicted;
        assert!(
            next_per_turn > first_per_turn,
            "the baseline drops another turn, so the history is rewritten again",
        );
        assert_eq!(
            next_block, first_block,
            "the block policy still fits, so the history is byte-identical",
        );
    }

    #[test]
    fn a_history_that_still_fits_is_untouched_by_either_policy() {
        let counter = WordCounter::default();
        let mut context = context_of_equal_turns(3, &counter);

        let selection = context.select(
            "q",
            &[],
            Budget::new(8192, 0, Eviction::Block { low_water: 0.5 }),
            &counter,
        );

        assert_eq!(
            selection.evicted, 0,
            "the low-water mark is a floor, not a cap"
        );
        assert_eq!(selection.messages.len(), 3 * 2 + 2);
    }

    #[test]
    fn the_policy_is_part_of_what_a_run_was_measured_under() {
        let json = serde_json::to_value(Eviction::Block { low_water: 0.5 }).unwrap();
        assert_eq!(json["policy"], "block");
        assert_eq!(json["low_water"], 0.5);
        assert_eq!(
            serde_json::to_value(Eviction::Turn).unwrap()["policy"],
            "turn"
        );
    }

    // ---- Rule A: what a span already in the window costs to select again ----
    //
    // `RECORD/2026-09-06.what-leaves-the-history.completed.md`. The corpus these were
    // written against is `scripts/tasks/grounded.txt`, whose consecutive turns
    // select overlapping spans; these are the mechanism underneath it.

    fn fragment(path: &str, text: &str) -> Fragment {
        Fragment {
            path: path.into(),
            text: text.into(),
        }
    }

    /// A context whose turns each carry the fragments they were given.
    fn context_carrying(spans: &[&[Fragment]], counter: &dyn TokenCounter) -> Context {
        let mut context = Context::new("system prompt here");
        for (n, code) in spans.iter().enumerate() {
            context.push_turn(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                code.to_vec(),
                counter,
            );
        }
        context
    }

    /// The fix of `RECORD/2026-09-20.the-newest-body-wins.completed.md`, which
    /// is the same corpus this test pinned the *defect* on for one day.
    ///
    /// Until format 15 this asserted that the prompt **does** contain the
    /// contradiction, because the repair was argued and deliberately not taken
    /// while the frequency it turns on had no number. The corpus produced the
    /// number — 8 of 13 renders — and this is the other side of that diff: one
    /// body goes out, it is the newest one, and the stale block is the line
    /// that cites it.
    #[test]
    fn an_edited_span_goes_out_once_and_the_stale_body_is_cited() {
        let counter = WordCounter::default();
        let before = fragment("src/lib.rs:1-4", "fn main () { old }");
        let after = fragment("src/lib.rs:1-4", "fn main () { new }");
        let mut context = context_carrying(
            &[std::slice::from_ref(&before), std::slice::from_ref(&after)],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );

        let users = user_messages(&selection);
        assert!(
            !users[0].contains("old"),
            "the body nothing on disk agrees with: {users:?}",
        );
        assert!(
            users[0].contains("src/lib.rs:1-4") && users[0].contains("superseded"),
            "the turn is still told it read the file, and which one: {users:?}",
        );
        assert!(
            users[1].contains("new"),
            "the newest body, in full: {users:?}"
        );

        assert!(
            selection.diverged.is_empty(),
            "one body is not a divergence: {:?}",
            selection.diverged,
        );
        assert_eq!(selection.superseded.len(), 1, "{:?}", selection.superseded);
        let one = &selection.superseded[0];
        assert_eq!(one.path, "src/lib.rs:1-4");
        assert_eq!(one.turns, vec![1], "the turn whose body was replaced");
        assert_eq!(one.spans, 1);
        assert!(one.tokens > 0, "the bytes it stood in for");
        assert_eq!(one.counter, counter.id(), "a count says who counted it");
    }

    /// The arm one flag apart, and the reason the repair is rule A's and not a
    /// fourth window rule.
    ///
    /// Under `Repeat::Always` a prompt carrying one path twice is the
    /// *documented* behaviour — every turn re-sends what it selected and the
    /// blocks read as per-turn snapshots — so nothing is superseded and the
    /// detector reports what it always did. This is the same corpus as above,
    /// and it is what a run made before format 15 still reads as.
    #[test]
    fn the_arm_without_rule_a_still_sends_both_bodies_and_reports_them() {
        let counter = WordCounter::default();
        let before = fragment("src/lib.rs:1-4", "fn main () { old }");
        let after = fragment("src/lib.rs:1-4", "fn main () { new }");
        let mut context = context_carrying(
            &[std::slice::from_ref(&before), std::slice::from_ref(&after)],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Always),
            &counter,
        );

        let users = user_messages(&selection);
        assert!(
            users[0].contains("old") && users[1].contains("new"),
            "{users:?}"
        );
        assert!(
            selection.superseded.is_empty(),
            "rule A's repair cannot reach an arm rule A is off in: {:?}",
            selection.superseded,
        );
        assert_eq!(selection.diverged.len(), 1, "{:?}", selection.diverged);
        let one = &selection.diverged[0];
        assert_eq!(one.path, "src/lib.rs:1-4");
        assert_eq!(one.bodies, 2);
        assert_eq!(one.turns, vec![1, 2]);
        assert!(!one.asking, "neither body belongs to the turn being asked");
    }

    /// The case the record calls the one that matters most: the history holds
    /// the old bytes and the turn being asked holds what is on disk now.
    ///
    /// It is also where the invariant behind [`Superseded`]'s missing `asking`
    /// field is visible. The turn being asked loaded its span through the
    /// sandbox in this very call, so it holds the newest read there is and is
    /// never the turn superseded — the history is. `Diverged` needs the `bool`
    /// because either side can be a carrier; this report never does.
    #[test]
    fn the_turn_being_asked_supersedes_what_the_history_holds() {
        let counter = WordCounter::default();
        let before = fragment("src/lib.rs:1-4", "fn main () { old }");
        let after = fragment("src/lib.rs:1-4", "fn main () { new }");
        let mut context = context_carrying(&[std::slice::from_ref(&before)], &counter);

        let selection = context.select(
            "now this",
            std::slice::from_ref(&after),
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );

        assert!(
            selection.diverged.is_empty(),
            "the prompt no longer disagrees with the disk: {:?}",
            selection.diverged,
        );
        assert_eq!(selection.superseded.len(), 1, "{:?}", selection.superseded);
        assert_eq!(
            selection.superseded[0].turns,
            vec![1],
            "the history's carrier, and only it",
        );

        let users = user_messages(&selection);
        assert!(!users[0].contains("old"), "{users:?}");
        assert!(
            users.last().expect("the asking turn").contains("new"),
            "what was read this turn goes out in full: {users:?}",
        );

        // And the arm one flag apart, where the same window is the defect the
        // detector was built for.
        let mut always = context_carrying(&[std::slice::from_ref(&before)], &counter);
        let selection = always.select(
            "now this",
            std::slice::from_ref(&after),
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Always),
            &counter,
        );
        assert_eq!(selection.diverged.len(), 1, "{:?}", selection.diverged);
        assert!(
            selection.diverged[0].asking,
            "the turn being asked, which has no id yet and is the reason \
             that is a bool rather than an id among them",
        );
    }

    /// The three ways a window carries one path without contradicting itself.
    /// A detector that fired on any of these would be noise, and the third is
    /// the one that makes it a detector of *what was sent* rather than of what
    /// is stored.
    #[test]
    fn an_unchanged_span_a_lone_span_and_a_pruned_one_are_not_divergences() {
        let counter = WordCounter::default();
        let same = fragment("src/lib.rs:1-4", "fn main () {}");

        // Carried by two turns with the same bytes: one body, whichever rule
        // renders it, so neither arm reports anything.
        let mut context = context_carrying(
            &[std::slice::from_ref(&same), std::slice::from_ref(&same)],
            &counter,
        );
        for repeat in [Repeat::Once, Repeat::Always] {
            let selection = context.select(
                "now this",
                &[],
                Budget::new(8192, 0, Eviction::Turn).repeating(repeat),
                &counter,
            );
            assert!(
                selection.diverged.is_empty(),
                "{repeat:?}: {:?}",
                selection.diverged,
            );
        }

        // One turn, one span: nothing to disagree with.
        let mut lone = context_carrying(&[std::slice::from_ref(&same)], &counter);
        let selection = lone.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );
        assert!(selection.diverged.is_empty());

        // And the one that decides what the detector is reading. The old body
        // is behind the prune line, so it is a citation rather than bytes — the
        // model is shown one body and there is nothing to be wrong about, even
        // though `code_context` still holds both.
        let before = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = one two three four }",
        );
        let after = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = five six seven eight }",
        );
        let mut pruning = context_carrying(
            &[
                std::slice::from_ref(&before),
                std::slice::from_ref(&before),
                std::slice::from_ref(&after),
            ],
            &counter,
        );
        let selection = pruning.select(
            "now this",
            &[],
            Budget::new(50, 0, Eviction::Turn).pruning(Prune::Behind),
            &counter,
        );
        assert!(selection.pruned > 0, "the point of the case");
        let rendered: usize = user_messages(&selection)
            .iter()
            .filter(|m| m.contains("one two three four"))
            .count();
        assert_eq!(
            rendered, 0,
            "the stale body was pruned to a citation, so it was never sent",
        );
        assert!(
            selection.diverged.is_empty(),
            "and a detector reading what was sent says nothing: {:?}",
            selection.diverged,
        );
    }

    /// Two files moving at once are two findings, and a third file that did
    /// not move is not one of them — on both sides of the flag.
    #[test]
    fn each_moved_path_is_its_own_finding() {
        let counter = WordCounter::default();
        let old_a = fragment("a.rs:1-2", "fn a () { old }");
        let new_a = fragment("a.rs:1-2", "fn a () { new }");
        let old_b = fragment("b.rs:1-2", "fn b () { old }");
        let new_b = fragment("b.rs:1-2", "fn b () { new }");
        let steady = fragment("c.rs:1-2", "fn c () {}");

        let mut context = context_carrying(
            &[
                &[old_a.clone(), old_b.clone(), steady.clone()],
                &[new_a.clone(), new_b.clone(), steady.clone()],
            ],
            &counter,
        );
        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );

        let paths: Vec<&str> = selection
            .superseded
            .iter()
            .map(|one| one.path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["a.rs:1-2", "b.rs:1-2"],
            "in the order the window first carried them, and c.rs is not among them",
        );
        assert!(selection.diverged.is_empty(), "{:?}", selection.diverged);

        let mut always = context_carrying(
            &[&[old_a, old_b, steady.clone()], &[new_a, new_b, steady]],
            &counter,
        );
        let selection = always.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Always),
            &counter,
        );
        let paths: Vec<&str> = selection
            .diverged
            .iter()
            .map(|one| one.path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["a.rs:1-2", "b.rs:1-2"],
            "the same two paths, as the defect rather than as the repair",
        );
        assert!(selection.superseded.is_empty());
    }

    /// A file edited, put back, and edited again — four carriers, three
    /// bodies, and after the fix exactly one of them goes out.
    ///
    /// The edit-back is why [`Current`] names the *newest body of a path* and
    /// not *every body ever found stale*. Turn 1's bytes were stale the moment
    /// turn 2 read something else and were true again at turn 3; a ratchet over
    /// bodies would have kept citing them, and cited the only body in the
    /// window that was on disk. What ratchets here is the reader — the newest
    /// turn carrying a path only ever moves forward, because turns are only
    /// ever appended.
    #[test]
    fn the_edit_and_the_edit_back_are_both_superseded_by_the_newest_read() {
        let counter = WordCounter::default();
        let one = fragment("src/lib.rs:1-4", "fn main () { one }");
        let two = fragment("src/lib.rs:1-4", "fn main () { two }");
        let three = fragment("src/lib.rs:1-4", "fn main () { three }");

        let mut context = context_carrying(
            &[
                std::slice::from_ref(&one),
                std::slice::from_ref(&two),
                // The first body again: a file edited and put back. Four
                // carriers, three bodies.
                std::slice::from_ref(&one),
                std::slice::from_ref(&three),
            ],
            &counter,
        );
        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );

        assert!(selection.diverged.is_empty(), "{:?}", selection.diverged);
        assert_eq!(selection.superseded.len(), 1, "{:?}", selection.superseded);
        assert_eq!(
            selection.superseded[0].turns,
            vec![1, 2],
            "turn 3 agreed with turn 1 byte for byte, so rule A deduped it and \
             it rendered nothing to supersede — and turn 4 is the newest read, \
             which is the one body that goes out",
        );
        assert_eq!(selection.superseded[0].spans, 2);

        let users = user_messages(&selection);
        for stale in ["{ one }", "{ two }"] {
            assert!(
                !users.iter().any(|text| text.contains(stale)),
                "{stale} is not what the newest read found: {users:?}",
            );
        }
        assert!(
            users.iter().any(|text| text.contains("{ three }")),
            "{users:?}",
        );
    }

    /// The ratchet, stated as the case that would break without one: a fresh
    /// read leaving the window does not put the stale bytes back.
    ///
    /// [`Current`] is derived over every turn the session holds, the evicted
    /// ones included, so what a turn *observed* outlives whether that turn is
    /// still being sent. The floor and the prune line each needed a stored
    /// field to get this; here it falls out of the derivation.
    #[test]
    fn an_evicted_fresh_read_does_not_put_the_stale_body_back() {
        let counter = WordCounter::default();
        let stale = fragment("src/lib.rs:1-4", "fn main () { old }");
        let fresh = fragment("src/lib.rs:1-4", "fn main () { new }");
        let mut context = context_carrying(
            &[std::slice::from_ref(&stale), std::slice::from_ref(&fresh)],
            &counter,
        );

        // Wide enough for both turns: the newest body goes out and the oldest
        // is cited.
        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );
        assert!(user_messages(&selection)[1].contains("new"));
        assert_eq!(selection.superseded.len(), 1);

        // Then a prompt that will not fit both, so the turn holding the fresh
        // read is the one that leaves. Only the stale carrier is left in the
        // window, and it is still not the truth.
        let long: String = std::iter::repeat_n("pad", 40).collect::<Vec<_>>().join(" ");
        let selection = context.select(&long, &[], Budget::new(60, 0, Eviction::Turn), &counter);
        assert!(selection.evicted > 0, "the point of the case");
        let users = user_messages(&selection);
        assert!(
            !users.iter().any(|text| text.contains("{ old }")),
            "a body the session watched go stale came back when its reader \
             left the window: {users:?}",
        );
    }

    /// The one place supersession and rule B's prune part company.
    ///
    /// A pruned span's citation is a *saving* and is not taken where it costs
    /// more than the span — a citation that is not cheaper is not a saving. A
    /// superseded span's citation is a *correction*: what it replaces is not
    /// expensive, it is untrue, and being cheap to send is not a reason to send
    /// bytes the session knows are stale.
    #[test]
    fn a_superseded_citation_is_taken_even_where_it_costs_more_than_the_span() {
        let counter = WordCounter::default();
        // Two words, so the citation naming the path and a token count is
        // dearer than the body it replaces.
        let stale = fragment("x.rs:1", "old");
        let fresh = fragment("x.rs:1", "new");
        assert!(
            counter.count(&superseded_fragment_text(&stale, &counter))
                > fragment_tokens(&stale, &counter),
            "the case only means something where the citation is the dearer half",
        );
        let mut context = context_carrying(
            &[std::slice::from_ref(&stale), std::slice::from_ref(&fresh)],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );
        let users = user_messages(&selection);
        assert!(!users[0].contains("old"), "{users:?}");
        assert!(users[0].contains("superseded"), "{users:?}");
    }

    /// `buckets` describes `messages` rather than estimating them, which is
    /// this file's own rule and the one a second render path would break.
    ///
    /// The history bucket has to carry the citations that went out, not the
    /// bytes they replaced — and the two differ by design, because the
    /// correction above is unguarded and can cost more than the span.
    #[test]
    fn the_history_bucket_counts_the_citations_and_not_the_bytes() {
        let counter = WordCounter::default();
        let stale = fragment("src/lib.rs:1-9", "fn main () { one two three four }");
        let fresh = fragment("src/lib.rs:1-9", "fn main () { five six seven eight }");
        let mut context = context_carrying(
            &[std::slice::from_ref(&stale), std::slice::from_ref(&fresh)],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );

        let history = selection
            .buckets
            .iter()
            .find(|bucket| bucket.name == "history")
            .expect("a history bucket")
            .tokens;
        let rendered: u32 = user_messages(&selection)
            .iter()
            .take(2)
            .map(|text| counter.count(text))
            .sum::<u32>()
            + selection
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .map(|m| counter.count(&m.content))
                .sum::<u32>();
        assert_eq!(
            history, rendered,
            "the bucket and the messages disagree, which is the drift this \
             file keeps one render path to avoid",
        );
    }

    /// The same corpus under the other arm, which is the control for the
    /// sentence above: `Repeat::Always` dedups nothing, so turn 3 does send its
    /// body and is named. Three bodies either way — the count is of distinct
    /// bodies and both arms sent all three.
    #[test]
    fn the_arm_decides_which_carriers_are_named_and_not_how_many_bodies() {
        let counter = WordCounter::default();
        let one = fragment("src/lib.rs:1-4", "fn main () { one }");
        let two = fragment("src/lib.rs:1-4", "fn main () { two }");
        let three = fragment("src/lib.rs:1-4", "fn main () { three }");

        let mut context = context_carrying(
            &[
                std::slice::from_ref(&one),
                std::slice::from_ref(&two),
                std::slice::from_ref(&one),
                std::slice::from_ref(&three),
            ],
            &counter,
        );
        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Always),
            &counter,
        );

        assert_eq!(selection.diverged.len(), 1);
        assert_eq!(selection.diverged[0].bodies, 3);
        assert_eq!(selection.diverged[0].turns, vec![1, 2, 3, 4]);
    }

    fn user_messages(selection: &Selection) -> Vec<&String> {
        selection
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| &m.content)
            .collect()
    }

    fn occurrences(selection: &Selection, needle: &str) -> usize {
        selection
            .messages
            .iter()
            .filter(|m| m.content.contains(needle))
            .count()
    }

    #[test]
    fn a_span_two_turns_carry_is_rendered_by_the_older_one() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let mut context = context_carrying(
            &[std::slice::from_ref(&shared), std::slice::from_ref(&shared)],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Once),
            &counter,
        );

        assert_eq!(occurrences(&selection, "src/lib.rs:1-4"), 1);
        let users = user_messages(&selection);
        assert!(
            users[0].contains("src/lib.rs:1-4"),
            "the oldest turn keeps it, so the block above the newest message \
             does not move when a later turn selects it again: {users:?}",
        );
        assert!(!users[1].contains("src/lib.rs:1-4"));
    }

    /// The arm that was the default until 2026-09-19, asked for by name.
    ///
    /// It stopped being what a `Budget` says when asked nothing and it did not
    /// stop being an arm: every recording made before that date was rendered
    /// this way, and a reader that cannot reproduce the arm cannot check the
    /// run. The test kept its assertion and gained one line — which is the
    /// whole of what the flip did to this rule's behaviour.
    #[test]
    fn asking_for_always_sends_it_in_every_turn_that_selected_it() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let mut context = context_carrying(
            &[std::slice::from_ref(&shared), std::slice::from_ref(&shared)],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Always),
            &counter,
        );

        assert_eq!(
            occurrences(&selection, "src/lib.rs:1-4"),
            2,
            "every recording on disk was made under this arm and must stay readable as it",
        );
    }

    /// And the same corpus under no instruction at all, which is the flip.
    #[test]
    fn a_budget_asked_nothing_now_sends_a_shared_span_once() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let mut context = context_carrying(
            &[std::slice::from_ref(&shared), std::slice::from_ref(&shared)],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn),
            &counter,
        );

        assert_eq!(
            occurrences(&selection, "src/lib.rs:1-4"),
            1,
            "rule A is what a run gets without asking, since 2026-09-19",
        );
    }

    #[test]
    fn the_turn_being_asked_is_the_one_that_gives_the_span_up() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let mut context = context_carrying(&[std::slice::from_ref(&shared)], &counter);

        let selection = context.select(
            "now this",
            std::slice::from_ref(&shared),
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Once),
            &counter,
        );

        assert_eq!(occurrences(&selection, "src/lib.rs:1-4"), 1);
        let last = selection.messages.last().unwrap();
        assert_eq!(
            last.content, "now this",
            "the youngest message is the one that changes anyway",
        );
        let code = selection.buckets.iter().find(|b| b.name == "code").unwrap();
        assert_eq!(code.tokens, 0, "the bucket has to say what was sent");
    }

    /// The failure this rule is one bad decision away from, and the reason the
    /// owner is chosen inside the window rather than over the session:
    /// `the-fold-fix-verified`, with a different mechanism.
    #[test]
    fn evicting_the_turn_that_owned_a_span_hands_it_to_the_next_one() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let mut context = context_carrying(&[std::slice::from_ref(&shared); 3], &counter);
        let repeat = Repeat::Once;

        // Wide enough for everything: turn 1 owns it.
        let first = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn).repeating(repeat),
            &counter,
        );
        assert!(user_messages(&first)[0].contains("fn main () {}"));

        // Narrow enough to lose turn 1. The span must not go with it.
        let second = context.select(
            "now this",
            &[],
            Budget::new(26, 0, Eviction::Turn).repeating(repeat),
            &counter,
        );
        assert!(second.evicted > 0, "the point of the case");
        assert_eq!(
            occurrences(&second, "src/lib.rs:1-4"),
            1,
            "still exactly once — and now in a turn that is still here",
        );
        assert!(
            user_messages(&second)[0].contains("fn main () {}"),
            "a model reasoning over code it can no longer see is the failure \
             this whole rule is built around",
        );
    }

    #[test]
    fn a_span_whose_bytes_changed_is_not_the_span_it_replaces() {
        let counter = WordCounter::default();
        let mut context = context_carrying(
            &[&[fragment("src/lib.rs:1-4", "fn main () {}")][..]],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[fragment("src/lib.rs:1-4", "fn main () { work () }")],
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Once),
            &counter,
        );

        assert_eq!(
            occurrences(&selection, "src/lib.rs:1-4"),
            2,
            "the same spec after an edit is a different claim — see \
             `resolving-a-symbol`, which is the live bug this would reintroduce",
        );
    }

    #[test]
    fn a_span_inside_a_folded_job_does_not_stand_in_for_one_on_the_screen() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let mut context = Context::new("system prompt here");
        let job = context.approved_job("read it", Plan::default());
        context.push_turn(1, "look at this", "looked", vec![shared.clone()], &counter);
        context.close_job_by(job, &counter, ClosedBy::User);
        context.push_turn(2, "and again", "again", vec![shared.clone()], &counter);

        let selection = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Once),
            &counter,
        );

        assert!(
            user_messages(&selection)
                .iter()
                .any(|text| text.contains("fn main () {}")),
            "a folded job sends its summary, not its turns: nothing inside one \
             is on the screen to stand in for anything — {:?}",
            user_messages(&selection),
        );
    }

    /// The buckets are what the panel plots and what a recording stores. A rule
    /// that drops bytes from the prompt without moving the bar would make every
    /// number after it a fiction.
    #[test]
    fn the_buckets_describe_what_was_actually_sent() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let mut context = context_carrying(
            &[std::slice::from_ref(&shared), std::slice::from_ref(&shared)],
            &counter,
        );

        let with = context.select(
            "now this",
            std::slice::from_ref(&shared),
            Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Once),
            &counter,
        );
        let bucket = |selection: &Selection, name: &str| {
            selection
                .buckets
                .iter()
                .find(|b| b.name == name)
                .unwrap()
                .tokens
        };

        // The word counter is additive across a concatenation, so this is an
        // equality here and would be within a token or two under a real
        // tokenizer — the gap the `code`/`prompt` split already carries. What
        // it pins is the bookkeeping: bytes dropped from the prompt are dropped
        // from the bar.
        let rendered: u32 = with
            .messages
            .iter()
            .map(|m| counter.count(&m.content))
            .sum();
        let plotted = bucket(&with, "system")
            + bucket(&with, "tools")
            + bucket(&with, "map")
            + bucket(&with, "summaries")
            + bucket(&with, "history")
            + bucket(&with, "code")
            + bucket(&with, "prompt");
        assert_eq!(rendered, plotted, "{:?}", with.buckets);
    }

    /// The guarantee that makes the rule safe to turn on: the cut is computed
    /// against a history that costs less, so it is never deeper than the one
    /// the same budget would have made without it.
    #[test]
    fn the_rule_never_evicts_more_than_the_default_would() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let carried = [std::slice::from_ref(&shared); 4];
        let mut kept_more = 0;

        for limit in 20..90 {
            // The baseline says `Always` out loud now that it is no longer what
            // `Budget::new` hands back. Without the line this compares the rule
            // against itself and the sweep below proves nothing — which is the
            // failure mode a flipped default has in every one-flag-apart test
            // in this file.
            let budget = Budget::new(limit, 0, Eviction::Turn).repeating(Repeat::Always);
            let mut plain = context_carrying(&carried, &counter);
            let mut once = context_carrying(&carried, &counter);

            let plain = plain.select("now this", &[], budget, &counter);
            let once = once.select("now this", &[], budget.repeating(Repeat::Once), &counter);

            assert!(
                once.evicted <= plain.evicted,
                "at limit {limit}: {} evicted with the rule against {} without it",
                once.evicted,
                plain.evicted,
            );
            kept_more += usize::from(once.evicted < plain.evicted);
        }

        assert!(
            kept_more > 0,
            "never worse is only half of it — a range of windows where the rule \
             changes nothing is a range that proves nothing",
        );
    }

    /// Rule A's whole claim against rule B: the history block does not move, so
    /// a prefix cache reuses everything above the newest message.
    #[test]
    fn a_later_repeat_does_not_rewrite_the_block_above_it() {
        let counter = WordCounter::default();
        let shared = fragment("src/lib.rs:1-4", "fn main () {}");
        let mut context = context_carrying(&[std::slice::from_ref(&shared)], &counter);
        let budget = Budget::new(8192, 0, Eviction::Turn).repeating(Repeat::Once);

        let before = context.select("first", &[], budget, &counter);
        // The next turn selects the same span, and answers.
        context.push_turn(2, "second", "answered", vec![shared.clone()], &counter);
        let after = context.select("third", &[], budget, &counter);

        assert_eq!(
            before.messages[..before.messages.len() - 1],
            after.messages[..before.messages.len() - 1],
            "everything the previous call sent above its own prompt is byte-identical",
        );
    }

    /// Rule B, and the sentence the whole record is: the exchange survives the
    /// cut its quoted code does not.
    #[test]
    fn a_pruned_turn_keeps_its_exchange_and_gives_up_its_code() {
        let counter = WordCounter::default();
        let span = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = one two three four }",
        );
        let mut context = context_carrying(
            &[
                std::slice::from_ref(&span),
                std::slice::from_ref(&span),
                &[],
            ],
            &counter,
        );

        // Narrow enough that the window does not fit, wide enough that the
        // conversation does once the code is a citation — which is the whole
        // trade: nothing is evicted here.
        // `Always` by name: this corpus is one span carried by three turns,
        // which is exactly what rule A collapses, so leaving the default in
        // place would measure the two rules together and call it rule B.
        let budget = Budget::new(50, 0, Eviction::Turn)
            .repeating(Repeat::Always)
            .pruning(Prune::Behind);
        let selection = context.select("now this", &[], budget, &counter);

        assert!(selection.pruned > 0, "the point of the case");
        assert_eq!(selection.evicted, 0, "and nothing had to be dropped whole");
        let users = user_messages(&selection);
        assert!(
            users[0].contains("src/lib.rs:1-9") && !users[0].contains("fn main"),
            "the citation stands where the span stood: {users:?}",
        );
        assert!(
            users[0].contains("question number 0"),
            "and the question is still being asked: {users:?}",
        );
        assert!(
            selection
                .messages
                .iter()
                .any(|m| m.content.contains("answer number 0")),
            "with the answer it got",
        );
    }

    /// Pruning is the cheaper half of the same trade, so it goes first: a
    /// window that fits once the old code is a citation loses no turn at all.
    #[test]
    fn pruning_comes_before_eviction() {
        let counter = WordCounter::default();
        let span = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = one two three four }",
        );
        let carried: Vec<&[Fragment]> = vec![std::slice::from_ref(&span); 3];

        let plain = Budget::new(40, 0, Eviction::Turn).repeating(Repeat::Always);
        let mut without = context_carrying(&carried, &counter);
        let without = without.select("now this", &[], plain, &counter);

        let mut with = context_carrying(&carried, &counter);
        let with = with.select("now this", &[], plain.pruning(Prune::Behind), &counter);

        assert!(without.evicted > 0, "the arm this is measured against");
        assert!(
            with.evicted < without.evicted,
            "a turn that gave up its code did not have to be dropped: {} against {}",
            with.evicted,
            without.evicted,
        );
    }

    /// A ratchet, for the reason the floor is one: a line that moved back would
    /// rewrite the history block twice and leave no prefix to reuse.
    #[test]
    fn the_prune_line_only_moves_forward() {
        let counter = WordCounter::default();
        let span = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = one two three four }",
        );
        let carried: Vec<&[Fragment]> = vec![std::slice::from_ref(&span); 3];
        let mut context = context_carrying(&carried, &counter);

        let narrow = context.select(
            "now this",
            &[],
            Budget::new(40, 0, Eviction::Turn)
                .repeating(Repeat::Always)
                .pruning(Prune::Behind),
            &counter,
        );
        assert!(narrow.pruned > 0, "the point of the case");

        let wide = context.select(
            "now this",
            &[],
            Budget::new(8192, 0, Eviction::Turn)
                .repeating(Repeat::Always)
                .pruning(Prune::Behind),
            &counter,
        );
        assert_eq!(
            wide.pruned, narrow.pruned,
            "room appearing again does not put the code back",
        );
    }

    #[test]
    fn the_default_prunes_nothing() {
        let counter = WordCounter::default();
        let span = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = one two three four }",
        );
        let carried: Vec<&[Fragment]> = vec![std::slice::from_ref(&span); 3];
        let mut context = context_carrying(&carried, &counter);

        let selection = context.select(
            "now this",
            &[],
            Budget::new(40, 0, Eviction::Turn),
            &counter,
        );

        assert_eq!(
            selection.pruned, 0,
            "every recording on disk was made under this arm and must stay readable as it",
        );
        assert_eq!(occurrences(&selection, "pruned"), 0);
    }

    /// The guard that keeps the window's cost monotone in the prune line: a
    /// citation is only taken where it is cheaper than what it replaces.
    #[test]
    fn a_citation_that_costs_more_than_the_span_is_not_taken() {
        let counter = WordCounter::default();
        let tiny = fragment("src/lib.rs:1-1", "ok");
        let mut context = context_carrying(
            &[std::slice::from_ref(&tiny), std::slice::from_ref(&tiny)],
            &counter,
        );

        let selection = context.select(
            "now this",
            &[],
            Budget::new(20, 0, Eviction::Turn).pruning(Prune::Behind),
            &counter,
        );

        let users = user_messages(&selection);
        assert!(
            users[0].contains("ok"),
            "two words do not get cheaper by being cited: {users:?}",
        );
    }

    /// §4 of the record, and the part most likely to be got wrong quietly: a
    /// render that sends less than it counted evicts on tokens nobody sent.
    #[test]
    fn the_history_bucket_says_what_a_pruned_window_actually_sent() {
        let counter = WordCounter::default();
        let span = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = one two three four }",
        );
        let carried: Vec<&[Fragment]> = vec![std::slice::from_ref(&span); 3];
        let mut context = context_carrying(&carried, &counter);

        let selection = context.select(
            "now this",
            &[],
            Budget::new(40, 0, Eviction::Turn)
                .repeating(Repeat::Always)
                .pruning(Prune::Behind),
            &counter,
        );
        assert!(selection.pruned > 0, "the point of the case");

        // Everything but the system block and the turn being asked, which are
        // their own buckets.
        let history: u32 = selection.messages[1..selection.messages.len() - 1]
            .iter()
            .map(|m| counter.count(&m.content))
            .sum();
        let bucket = selection
            .buckets
            .iter()
            .find(|b| b.name == "history")
            .unwrap();
        assert_eq!(
            bucket.tokens, history,
            "the bar has to be a count of the prompt that went out",
        );
    }

    /// A and B are independent flags whose combination is defined rather than
    /// accidental — and the definition is not the one the record predicted.
    /// Where rule A has already reduced a span to one copy, pruning the turn
    /// that holds it hands the copy to a younger turn and adds a citation where
    /// it stood, which is bigger. So B declines, and the span is still rendered
    /// exactly once, by a turn that is still there.
    #[test]
    fn pruning_declines_where_rule_a_already_took_the_saving() {
        let counter = WordCounter::default();
        let span = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = one two three four }",
        );
        let carried: Vec<&[Fragment]> = vec![std::slice::from_ref(&span); 3];
        let mut context = context_carrying(&carried, &counter);

        let both = Budget::new(40, 0, Eviction::Turn)
            .repeating(Repeat::Once)
            .pruning(Prune::Behind);
        let selection = context.select("now this", &[], both, &counter);

        assert_eq!(
            selection.pruned, 0,
            "there is no second copy left for B to find",
        );
        let users = user_messages(&selection);
        let rendering = users.iter().filter(|user| user.contains("fn main")).count();
        assert_eq!(
            rendering, 1,
            "and the one copy is in a turn that is still here: {users:?}",
        );
    }

    /// One tool call and what came back, for the results rule below.
    fn ran(output: &str) -> ToolStep {
        ToolStep {
            text: "let me look".into(),
            call: ToolCall {
                name: "run_command".into(),
                arguments: serde_json::json!({}),
            },
            outcome: ToolOutcome::ok(Verdict::allow("test", Applied::Process), output),
            duration_ms: 1,
            asked: None,
        }
    }

    /// Turns that each ran one tool and selected nothing at all — the case rule
    /// B cannot reach, which is the whole reason there is a third rule.
    fn context_running(outputs: &[&str], counter: &dyn TokenCounter) -> Context {
        let mut context = Context::new("system prompt here");
        for (n, output) in outputs.iter().enumerate() {
            context.push_turn_with_steps(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                vec![],
                vec![ran(output)],
                counter,
            );
        }
        context
    }

    /// A step that read a file, beside `ran`'s, which ran a command.
    fn read(output: &str) -> ToolStep {
        ToolStep {
            text: "let me look".into(),
            call: ToolCall {
                name: "read_file".into(),
                arguments: serde_json::json!({}),
            },
            outcome: ToolOutcome::ok(Verdict::allow("test", Applied::Process), output),
            duration_ms: 1,
            asked: None,
        }
    }

    /// The cut `CitedReads` makes, over the registry that exists.
    ///
    /// **The count is asserted on purpose.** A sixth tool would otherwise land
    /// on the safe side of this function in silence, which is the right
    /// default and the wrong way to arrive at it: what a new tool's output *is*
    /// is a decision, and this is where somebody is made to take it.
    #[test]
    fn a_tool_output_is_evidence_unless_it_could_be_read_again() {
        let tools = crate::tools::Tools::standard();
        let names: Vec<&'static str> = tools.names().collect();
        assert_eq!(
            names.len(),
            5,
            "a tool joined the registry: say whether its output could be read again, \
             then move this number — {names:?}",
        );

        for name in ["read_file", "list_dir"] {
            assert!(
                could_be_read_again(name),
                "{name} describes something that is still there, so the model can look again",
            );
        }
        for name in ["run_command", "write_file", "edit_file"] {
            assert!(
                !could_be_read_again(name),
                "{name} is not a view of something still there",
            );
        }
        assert!(
            !could_be_read_again("some_tool_nobody_has_written_yet"),
            "an unknown tool keeps its output: arriving as cheap-to-lose is the failure \
             rule C's own flag was split off to avoid, one level down",
        );
    }

    /// The whole of part 5, in one window: the same prune line, the same turn,
    /// and two results that are not the same kind of thing.
    ///
    /// The number rule C is justified by was measured on 20 `read_file` calls
    /// against the 8 KiB cap and on no command output at all, so this value
    /// ships that saving and makes none of the claim item 5 is open about.
    #[test]
    fn citing_reads_gives_up_the_file_and_keeps_what_the_command_found() {
        let counter = WordCounter::default();
        let bytes = "one two three four five six seven eight nine ten";
        let found = "exit 0 all twenty tests passed in four seconds flat";

        // Three turns, each of which read a file *and* ran a command.
        let mut context = Context::new("system prompt here");
        for n in 0..3 {
            context.push_turn_with_steps(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                vec![],
                vec![read(bytes), ran(found)],
                &counter,
            );
        }

        let budget = Budget::new(90, 0, Eviction::Turn)
            .pruning(Prune::Behind)
            .citing(Results::CitedReads);
        let selection = context.select("now this", &[], budget, &counter);
        assert!(selection.pruned > 0, "the point of the case");

        let sent: String = selection
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            sent.contains("[read_file] output"),
            "the file's bytes are behind the line and can be read again: {sent}",
        );
        assert!(
            sent.contains(found),
            "and what the command found is still in the prompt — an exit code and a test \
             run are a record of something that happened, not a view of something still \
             there: {sent}",
        );
        assert!(
            !sent.contains("[run_command] output"),
            "so nothing cites it: {sent}",
        );

        // And the arm beside it, on the same corpus, which is what makes this a
        // choice rather than a refinement: `Cited` takes both.
        let mut every = Context::new("system prompt here");
        for n in 0..3 {
            every.push_turn_with_steps(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                vec![],
                vec![read(bytes), ran(found)],
                &counter,
            );
        }
        let all = every.select("now this", &[], budget.citing(Results::Cited), &counter);
        let sent: String = all
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            sent.contains("[run_command] output"),
            "`Cited` is unchanged and still takes a command's output — there is a run on \
             disk under it: {sent}",
        );
    }

    /// The reason the render and the accounting go through one function.
    ///
    /// Under two values each site could carry its own `match` and stay honest.
    /// Under three it is a per-tool question, and a second copy of it would be
    /// a window whose reported cost and actual cost differ by exactly one
    /// tool's output — silently, and only on a corpus that mixes the two.
    #[test]
    fn the_history_bucket_says_what_a_part_cited_window_actually_sent() {
        let counter = WordCounter::default();
        let bytes = "one two three four five six seven eight nine ten";
        let found = "exit 0 all twenty tests passed in four seconds flat";

        let mut context = Context::new("system prompt here");
        for n in 0..3 {
            context.push_turn_with_steps(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                vec![],
                vec![read(bytes), ran(found)],
                &counter,
            );
        }

        let selection = context.select(
            "now this",
            &[],
            Budget::new(90, 0, Eviction::Turn)
                .pruning(Prune::Behind)
                .citing(Results::CitedReads),
            &counter,
        );
        assert!(selection.pruned > 0, "the point of the case");

        let history: u32 = selection.messages[1..selection.messages.len() - 1]
            .iter()
            .map(|m| counter.count(&m.content))
            .sum();
        let bucket = selection
            .buckets
            .iter()
            .find(|b| b.name == "history")
            .unwrap();
        assert_eq!(bucket.tokens, history);
    }

    /// The rule, in one case: the call is the model's own words and stays, the
    /// output is bytes nobody has looked at for two turns and becomes the line
    /// that cites it.
    #[test]
    fn a_pruned_turn_keeps_the_call_and_gives_up_the_output() {
        let counter = WordCounter::default();
        let output = "one two three four five six seven eight nine ten";
        let mut context = context_running(&[output, output, output], &counter);

        let budget = Budget::new(70, 0, Eviction::Turn)
            .pruning(Prune::Behind)
            .citing(Results::Cited);
        let selection = context.select("now this", &[], budget, &counter);

        assert!(selection.pruned > 0, "the point of the case");
        assert_eq!(selection.evicted, 0, "and nothing had to be dropped whole");
        let users = user_messages(&selection);
        assert!(
            users[1].contains("[run_command] output —") && users[1].contains("pruned"),
            "the citation stands where the output stood: {users:?}",
        );
        assert!(
            !users[1].contains("seven"),
            "and the output itself is gone: {users:?}",
        );
        assert!(
            selection
                .messages
                .iter()
                .any(|m| m.role == Role::Assistant && m.content == "let me look"),
            "the call that asked for it is the model's own words and is untouched",
        );
        assert!(
            users[0].contains("question number 0"),
            "the question is still being asked: {users:?}",
        );
        for pair in selection.messages[1..].windows(2) {
            assert_ne!(pair[0].role, pair[1].role, "the alternation survives it");
        }
    }

    /// One flag apart, on the corpus rule B was built for and cannot help:
    /// without it the window drops a turn whole, with it the conversation is
    /// still there and only the bytes have gone.
    #[test]
    fn a_cited_result_saves_the_turn_that_ran_the_tool() {
        let counter = WordCounter::default();
        let output = "one two three four five six seven eight nine ten";
        let carried = [output, output, output];
        let behind = Budget::new(70, 0, Eviction::Turn).pruning(Prune::Behind);

        let mut without = context_running(&carried, &counter);
        let without = without.select("now this", &[], behind, &counter);
        let mut with = context_running(&carried, &counter);
        let with = with.select("now this", &[], behind.citing(Results::Cited), &counter);

        assert!(
            without.evicted > 0 && without.pruned == 0,
            "rule B has nothing to take from a turn that selected nothing:              {} evicted, line at {}",
            without.evicted,
            without.pruned,
        );
        assert_eq!(
            with.evicted, 0,
            "and the turn that was dropped is still in the window",
        );
        assert!(with.pruned > 0, "having given up its output instead");
    }

    /// Every recording on disk was made under a flag that did not exist, and it
    /// has to stay readable as the arm it was: this one is inert until the line
    /// it hangs from is asked for.
    #[test]
    fn citing_results_is_inert_without_a_prune_line() {
        let counter = WordCounter::default();
        let output = "one two three four five six seven eight nine ten";
        let carried = [output, output, output];
        let plain = Budget::new(70, 0, Eviction::Turn);

        let mut without = context_running(&carried, &counter);
        let without = without.select("now this", &[], plain, &counter);
        let mut with = context_running(&carried, &counter);
        let with = with.select("now this", &[], plain.citing(Results::Cited), &counter);

        assert_eq!(with.pruned, 0, "nothing is behind a line that never moved");
        assert_eq!(
            with.messages, without.messages,
            "and the prompt is byte-identical to the arm without the flag",
        );
    }

    /// The same guard a span's citation has, for the same reason: a line that
    /// is not cheaper than what it replaces is not a saving.
    #[test]
    fn a_result_too_small_to_cite_is_left_alone() {
        let counter = WordCounter::default();
        let span = fragment(
            "src/lib.rs:1-9",
            "fn main () { let a = one two three four }",
        );
        let mut context = Context::new("system prompt here");
        for n in 0..3 {
            context.push_turn_with_steps(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                vec![span.clone()],
                vec![ran("ok")],
                &counter,
            );
        }

        let selection = context.select(
            "now this",
            &[],
            Budget::new(60, 0, Eviction::Turn)
                .repeating(Repeat::Always)
                .pruning(Prune::Behind)
                .citing(Results::Cited),
            &counter,
        );

        assert!(selection.pruned > 0, "the span is what moved the line");
        let users = user_messages(&selection);
        assert!(
            users[1] == "[run_command] ok\nok",
            "three words do not get cheaper by being cited: {users:?}",
        );
    }

    /// The report has to be able to say why a turn is behind the line. Under
    /// this rule a turn that selected nothing can be, which the span-only
    /// filter would have left out of its own tombstone.
    #[test]
    fn the_report_names_a_turn_that_only_ran_a_tool() {
        let counter = WordCounter::default();
        let output = "one two three four five six seven eight nine ten";
        let mut context = context_running(&[output, output, output], &counter);

        let selection = context.select(
            "now this",
            &[],
            Budget::new(70, 0, Eviction::Turn)
                .pruning(Prune::Behind)
                .citing(Results::Cited),
            &counter,
        );

        let pruning = selection.pruning.expect("the line moved");
        assert_eq!(
            pruning.turns,
            vec![1, 2],
            "named rather than counted, and named though they carry no spans",
        );
        assert!(pruning.tokens > 0, "and the window is smaller for it");
    }

    /// The bar has to be a count of the prompt that went out — the failure that
    /// would otherwise evict on tokens nobody sent.
    #[test]
    fn the_history_bucket_says_what_a_cited_window_actually_sent() {
        let counter = WordCounter::default();
        let output = "one two three four five six seven eight nine ten";
        let mut context = context_running(&[output, output, output], &counter);

        let selection = context.select(
            "now this",
            &[],
            Budget::new(70, 0, Eviction::Turn)
                .pruning(Prune::Behind)
                .citing(Results::Cited),
            &counter,
        );
        assert!(selection.pruned > 0, "the point of the case");

        let history: u32 = selection.messages[1..selection.messages.len() - 1]
            .iter()
            .map(|m| counter.count(&m.content))
            .sum();
        let bucket = selection
            .buckets
            .iter()
            .find(|b| b.name == "history")
            .unwrap();
        assert_eq!(bucket.tokens, history);
    }

    /// The bug this rule made visible, and it arrived with rule B rather than
    /// with this one: a fold counted what its turns were *stored* at, and a
    /// pruned turn is in the prompt as its citations. The fold was claiming a
    /// saving the prune line had already taken.
    #[test]
    fn a_fold_does_not_claim_back_what_the_prune_line_already_took() {
        let counter = WordCounter::default();
        let output = "one two three four five six seven eight nine ten";
        let mut context = Context::new("system prompt here");
        let job = context.approved_job("run the tests", Plan::default());
        for n in 0..3 {
            context.push_turn_with_steps(
                n as TurnId + 1,
                format!("question number {n} padded out"),
                format!("answer number {n} padded out"),
                vec![],
                vec![ran(output)],
                &counter,
            );
        }

        let selection = context.select(
            "now this",
            &[],
            Budget::new(70, 0, Eviction::Turn)
                .pruning(Prune::Behind)
                .citing(Results::Cited),
            &counter,
        );
        assert!(selection.pruned > 0, "the fold has to land on pruned turns");

        context.close_job(job, &counter);
        let replaced = context.replaced_by(job).expect("the close counted it");

        let stored: u32 = context
            .turns
            .iter()
            .filter(|turn| turn.job == job)
            .map(|turn| context.tokens_of(turn, &counter))
            .sum();
        let current = Current::of(&context.turns, &[], context.resending);
        let sent: u32 = (0..context.turns.len())
            .map(|index| {
                context.item_tokens(
                    &Item::Turn(index),
                    &counter,
                    context.pruned,
                    context.cited,
                    &current,
                )
            })
            .sum();
        assert!(
            replaced.tokens < stored,
            "a pruned turn was not costing what it is stored at: {} against {}",
            replaced.tokens,
            stored,
        );
        assert_eq!(
            replaced.tokens, sent,
            "what the fold stopped sending is what the prompt was made of",
        );
    }
}

#[cfg(test)]
mod tool_turn_tests {
    use super::*;
    use crate::backend::Role;
    use crate::job::{JobState, PlanSource};
    use crate::sandbox::{Applied, Verdict};
    use crate::tools::{ToolCall, ToolOutcome, ToolStep};

    fn step(name: &str, text: &str, output: &str) -> ToolStep {
        ToolStep {
            text: text.into(),
            call: ToolCall {
                name: name.into(),
                arguments: serde_json::json!({}),
            },
            outcome: ToolOutcome::ok(Verdict::allow("test", Applied::Process), output),
            duration_ms: 1,
            asked: None,
        }
    }

    /// A step that ran a command and came back with an exit code, which is the
    /// only kind `close_if_met` looks at.
    fn ran(command: &str, args: &[&str], exit_code: Option<i32>) -> ToolStep {
        let mut step = step("run_command", "running it", "");
        let argv: Vec<&str> = std::iter::once(command)
            .chain(args.iter().copied())
            .collect();
        step.call.arguments = serde_json::json!({"argv": argv});
        step.outcome.command = Some(crate::tools::CommandResult {
            exit_code,
            signal: None,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 1,
        });
        step
    }

    /// An open task whose plan closes on `cargo test`, and one turn that ran
    /// the command with the exit code given.
    fn task_that_ran(exit_code: Option<i32>) -> (Context, TaskId) {
        let counter = ApproximateCounter;
        let mut context = Context::new("system");
        let task = context.approved_job(
            "make the tests pass",
            Plan {
                commands: vec!["cargo".into()],
                closes_on: Some("cargo test".into()),
                ..Plan::default()
            },
        );
        context.push_turn_with_steps(
            1,
            "fix the failing test",
            "Fixed.",
            vec![],
            vec![ran("cargo", &["test"], exit_code)],
            &counter,
        );
        (context, task)
    }

    #[test]
    fn a_green_run_closes_the_task_and_says_which_authority_did() {
        let (mut context, task) = task_that_ran(Some(0));
        let closed = context.close_if_met(&ApproximateCounter);

        assert_eq!(closed.map(|(id, _)| id), Some(task));
        let task = context.task(task).expect("the task");
        assert!(task.is_closed());
        assert_eq!(
            task.closed_by,
            Some(ClosedBy::ExitCode),
            "a recording where both closes look the same cannot count either",
        );
    }

    #[test]
    fn a_red_run_leaves_it_open_and_leaves_the_person_the_authority() {
        let (mut context, task) = task_that_ran(Some(1));
        assert!(context.close_if_met(&ApproximateCounter).is_none());
        assert!(context.task(task).expect("the task").is_open());

        // And the rung below still works on it, which is the whole point of the
        // condition being opt-in: nothing was taken away from the person.
        assert!(context.close_task(task, &ApproximateCounter).is_some());
        assert_eq!(
            context.task(task).expect("the task").closed_by,
            Some(ClosedBy::User),
        );
    }

    #[test]
    fn another_tasks_green_run_is_not_this_ones_evidence() {
        let counter = ApproximateCounter;
        let (mut context, first) = task_that_ran(Some(0));
        context.close_if_met(&counter);

        let second = context.approved_job(
            "something else",
            Plan {
                commands: vec!["cargo".into()],
                closes_on: Some("cargo test".into()),
                ..Plan::default()
            },
        );
        context.push_turn(2, "and now this", "Looking.", vec![], &counter);

        assert!(
            context.close_if_met(&counter).is_none(),
            "the first task's steps closed the first task and nothing else",
        );
        assert!(context.task(first).expect("the task").is_closed());
        assert!(context.task(second).expect("the task").is_open());
    }

    #[test]
    fn tool_definitions_are_in_the_prefix_and_counted_apart_from_the_system_text() {
        let mut context = Context::new("sys").with_tools("TOOLS");
        let selection = context.select(
            "hola",
            &[],
            Budget::new(8192, 512, Eviction::Turn),
            &ApproximateCounter,
        );

        assert_eq!(selection.messages[0].role, Role::System);
        assert!(selection.messages[0].content.contains("TOOLS"));
        let tools = selection
            .buckets
            .iter()
            .find(|b| b.name == "tools")
            .unwrap();
        assert!(
            tools.tokens > 0,
            "a bucket the definitions are hidden inside cannot explain a full window"
        );
    }

    #[test]
    fn a_turn_with_tool_calls_renders_as_alternating_messages() {
        let mut context = Context::new("sys");
        context.push_turn_with_steps(
            1,
            "read the file",
            "It defines main.",
            vec![],
            vec![step(
                "read_file",
                "let me look\n```tool\n{}\n```",
                "fn main() {}",
            )],
            &ApproximateCounter,
        );

        let selection = context.select(
            "and now?",
            &[],
            Budget::new(0, 0, Eviction::Turn),
            &ApproximateCounter,
        );
        let roles: Vec<Role> = selection.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [
                Role::System,
                Role::User,
                Role::Assistant,
                Role::User,
                Role::Assistant,
                Role::User
            ],
            "the call and its result are a real exchange, so nothing has to \
             decide what two user messages in a row mean",
        );
        assert!(selection.messages[3].content.contains("fn main() {}"));
    }

    #[test]
    fn a_turn_leaves_the_window_with_its_tool_calls() {
        // Half a turn would leave a result whose call has gone, which reads as
        // output nobody asked for.
        let counter = ApproximateCounter;
        let mut context = Context::new("sys");
        for n in 0..4 {
            context.push_turn_with_steps(
                n as TurnId + 1,
                format!("question {n} padded out a little"),
                format!("answer {n} padded out a little"),
                vec![],
                vec![step("read_file", "calling", &"x".repeat(200))],
                &counter,
            );
        }

        let selection = context.select("now", &[], Budget::new(256, 32, Eviction::Turn), &counter);
        assert!(
            selection.evicted > 0,
            "the results are what filled the window"
        );
        for pair in selection.messages[1..].windows(2) {
            assert_ne!(
                pair[0].role, pair[1].role,
                "the alternation survives eviction"
            );
        }
    }

    #[test]
    fn the_steps_are_counted_into_the_turn_they_belong_to() {
        let counter = ApproximateCounter;
        let mut bare = Context::new("sys");
        bare.push_turn(1, "q", "a", vec![], &counter);
        let mut with_steps = Context::new("sys");
        with_steps.push_turn_with_steps(
            1,
            "q",
            "a",
            vec![],
            vec![step("read_file", "calling", &"x".repeat(400))],
            &counter,
        );

        assert!(
            with_steps.turns()[0].tokens > bare.turns()[0].tokens,
            "a turn whose tool output is not in its count is a turn the budget cannot see"
        );
    }

    /// The lifecycle is a state machine, and the messages that drive it arrive
    /// from a socket anyone can open. Every transition that is not legal has to
    /// be refused rather than performed — found by driving the real page:
    /// closing a *proposed* task took the gate off the screen with its prompt
    /// still held, and the session had no way back.
    ///
    /// Half the illegal transitions of the old shape are gone because the
    /// states they targeted are: there is no proposal to close and no rejection
    /// to reopen, because neither is a job. What is left is the pair that still
    /// exists, and the alternation's own new one.
    #[test]
    fn only_the_legal_job_transitions_happen() {
        let counter = ApproximateCounter;
        let mut context = Context::new("system");
        context.push_turn(1, "have a look", "looking", Vec::new(), &counter);
        let draft = context.current_job().expect("a turn opens a draft");
        assert!(
            context.job(draft).unwrap().is_draft(),
            "a session opens with a draft, and its plan is the part nobody has written",
        );

        assert!(!context.reopen_job(draft), "it was never closed");
        assert!(context.close_job(draft, &counter).is_some());
        assert!(
            context.close_job(draft, &counter).is_none(),
            "closing a closed job would rewrite the summary the model already has",
        );
        assert!(context.reopen_job(draft));
        assert_eq!(context.job(draft).unwrap().state, JobState::Open);
    }

    /// Approving *is* closing: one call, two jobs, and the draft folds on the
    /// way through. The whole alternation in one assertion block.
    #[test]
    fn approving_closes_the_draft_and_opens_the_plan() {
        let counter = ApproximateCounter;
        let mut context = Context::new("system");
        context.push_turn(
            1,
            "what would it take",
            "this and that",
            Vec::new(),
            &counter,
        );
        let draft = context.current_job().expect("a draft");

        let approved = context.approve_plan(
            "add a flag",
            Plan::default(),
            Plan::default(),
            PlanSource::Model,
            ApprovedBy::Operator,
            &counter,
        );
        assert_eq!(approved.folded.as_ref().map(|(job, _)| *job), Some(draft));
        assert!(context.job(draft).unwrap().is_closed());
        assert_eq!(context.current_job(), Some(approved.job));
        assert!(
            !context.job(approved.job).unwrap().is_draft(),
            "the job an approval opens is the plan, and it has one",
        );

        context.push_turn(2, "do it", "doing it", Vec::new(), &counter);
        assert_eq!(
            context.turns()[1].job,
            approved.job,
            "a turn after the approval is work, not drafting",
        );
    }

    /// The happy path never makes a zero-turn draft, because a draft that
    /// receives no turn is never opened. Two messages of noise in the window,
    /// which §eighth of the window-rules record named and this rule removes.
    #[test]
    fn a_draft_nobody_asked_anything_in_is_never_opened() {
        let counter = ApproximateCounter;
        let mut context = Context::new("system");
        let approved = context.approve_plan(
            "add a flag",
            Plan::default(),
            Plan::default(),
            PlanSource::Written,
            ApprovedBy::Operator,
            &counter,
        );
        assert_eq!(approved.folded, None, "there was no draft to fold");
        assert_eq!(approved.job, 1, "and no draft took an id either");
        assert_eq!(context.jobs().len(), 1);
    }

    /// A declined plan never becomes a job, and never takes an id — which is
    /// what retired the reason `Rejected` was kept as a state. The round it
    /// leaves behind is the number that reason was about.
    #[test]
    fn a_declined_plan_is_a_round_and_not_a_job() {
        let counter = ApproximateCounter;
        let mut context = Context::new("system");
        context.push_turn(
            1,
            "delete everything",
            "that seems unwise",
            Vec::new(),
            &counter,
        );
        let draft = context.current_job().expect("a draft");

        context.decline_plan("delete everything", Plan::default(), PlanSource::Model);
        assert_eq!(context.jobs().len(), 1, "nothing was opened by a refusal");
        assert_eq!(
            context.current_job(),
            Some(draft),
            "declining ends nothing: you are still in the draft",
        );

        let approved = context.approve_plan(
            "delete one thing",
            Plan::default(),
            Plan::default(),
            PlanSource::Model,
            ApprovedBy::Operator,
            &counter,
        );
        assert_eq!(
            approved.job, 2,
            "the approval hands out the id the refusal did not"
        );
        assert_eq!(context.rounds().len(), 2, "both rounds are kept");
        assert_eq!(context.rounds()[0].answer, Some(Answer::Declined));
        assert_eq!(
            context.rounds()[1].answer,
            Some(Answer::Approved { job: 2 }),
        );
    }

    /// The sharp one, and it survives its cause: a reopen must not be able to
    /// make a job live that the alternation has moved past. It used to be a
    /// *rejected* job coming back as approved; there are no rejected jobs now,
    /// so what is left is reopening behind the front — job 1 while job 2 is
    /// open — which would give the session two open jobs and the next prompt a
    /// choice of two plans to run under.
    #[test]
    fn a_job_behind_the_front_cannot_be_reopened_into_a_live_one() {
        let counter = ApproximateCounter;
        let mut context = Context::new("system");
        context.push_turn(1, "have a look", "looking", Vec::new(), &counter);
        let draft = context.current_job().expect("a draft");
        let approved = context.approve_plan(
            "add a flag",
            Plan::default(),
            Plan::default(),
            PlanSource::Model,
            ApprovedBy::Operator,
            &counter,
        );

        assert!(
            !context.reopen_job(draft),
            "the draft is closed, but job 2 is open in front of it",
        );
        assert!(context.job(draft).unwrap().is_closed());
        assert_eq!(
            context.current_job(),
            Some(approved.job),
            "and the front is still the only thing that is live",
        );
    }
}

/// Resuming a stored session: what comes back, what cannot, and what is said
/// about the difference.
///
/// Its own module because it is the only group here that needs a **filesystem**:
/// a fragment restored by reference is one that was read again, so these are
/// the tests that cannot be written against a `Vec<Fragment>` somebody made up.
#[cfg(test)]
mod resume_tests {
    use super::*;

    /// A tree, a sandbox over it, and a view of one turn grounded with one span.
    ///
    /// The view is built by folding the two protocol messages a grounded turn
    /// produces, rather than by filling a `TurnView` in by hand: a fixture that
    /// reaches past the fold proves the fold's consumer and not the fold.
    struct ResumeFixture {
        root: std::path::PathBuf,
        sandbox: crate::sandbox::Sandbox,
    }

    impl ResumeFixture {
        /// `now` is what is on disk when the resume runs. Nothing anywhere
        /// holds what was on disk when the turn ran, which is the point.
        fn new(name: &str, now: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "luu-resume-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("greeting.txt"), now).unwrap();
            let root = root.canonicalize().unwrap();
            let sandbox = crate::sandbox::Sandbox::new(
                &crate::sandbox::SandboxPolicy {
                    paths: vec![crate::sandbox::PathRule::new(
                        ".",
                        crate::sandbox::Access::Read,
                    )],
                    ..crate::sandbox::SandboxPolicy::default()
                },
                &root,
            )
            .unwrap();
            Self { root, sandbox }
        }

        fn view(&self, spec: &str, origin: crate::protocol::Origin) -> crate::api::SessionView {
            let mut view = crate::api::SessionView::new("resumed", "mock", "mock");
            view.apply_protocol(
                0,
                &crate::protocol::ServerMessage::TurnStarted {
                    turn: 1,
                    prompt: "what does it say?".into(),
                    job: None,
                },
            );
            view.apply_protocol(
                0,
                &crate::protocol::ServerMessage::Grounded {
                    turn: 1,
                    spans: vec![crate::protocol::Grounding {
                        spec: spec.to_string(),
                        origin,
                    }],
                },
            );
            view
        }

        fn restore(&self, view: &crate::api::SessionView, with: bool) -> Restored {
            Context::from_view(
                view,
                "system",
                "tools",
                "",
                &ApproximateCounter,
                with.then_some(&self.sandbox),
            )
        }
    }

    impl Drop for ResumeFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A resumed turn gets its code back by reading it again, never from a store.
    ///
    /// Item 20 in one assertion: the view carries the spec and no bytes, the
    /// file on disk has changed since the turn that was grounded with it, and
    /// what comes back is **what is there now**. A resume that restored bytes
    /// would have put the old ones into a prompt built under a posture that
    /// never approved reading them.
    ///
    /// The freshness is a consequence and not a fix: from the next turn on, the
    /// session goes stale again exactly as a live one does. See
    /// `RECORD/2026-09-19.fragments-by-reference.completed.md`.
    #[test]
    fn a_resumed_turn_reads_its_spans_again_rather_than_getting_its_old_bytes() {
        let fixture = ResumeFixture::new("fresh", "after\n");
        let view = fixture.view("greeting.txt:1-1", crate::protocol::Origin::Selected);
        let restored = fixture.restore(&view, true);

        assert!(restored.unreadable.is_empty(), "{:?}", restored.unreadable);
        let code = &restored.context.turns[0].code_context;
        assert_eq!(code.len(), 1);
        assert_eq!(
            code[0].path, "greeting.txt:1-1",
            "the spec, as the turn named it"
        );
        assert_eq!(
            code[0].text, "after\n",
            "the bytes on disk now, not the ones the turn saw",
        );
    }

    /// No sandbox restores nothing, and that is the old behaviour on purpose.
    ///
    /// A caller with no filesystem in hand — the fold's own tests, a reader
    /// folding a recording to look at it — gets turns without code, which is
    /// what every resume did before this. It is not a fallback to *the bytes we
    /// had*: there are none, and the whole point is that there never will be.
    #[test]
    fn a_resume_with_no_sandbox_restores_no_code_rather_than_stale_code() {
        let fixture = ResumeFixture::new("nosandbox", "after\n");
        let view = fixture.view("greeting.txt:1-1", crate::protocol::Origin::Selected);
        let restored = fixture.restore(&view, false);

        assert!(restored.context.turns[0].code_context.is_empty());
        assert!(
            restored.unreadable.is_empty(),
            "not reading is not the same as failing to read: {:?}",
            restored.unreadable,
        );
    }

    /// A span this posture may not read comes back as a citation, and is named.
    ///
    /// The case that decided against storing bytes: a session resumed under a
    /// narrower posture must not get back a file it is no longer allowed to
    /// open. What it gets instead is the line saying the span was there, so the
    /// turn keeps its exchange and the model is told that reading the file
    /// again would be a decision rather than a discovery — and the resume says
    /// so out loud, because a refusal nobody can read is 2026-09-17's finding.
    #[test]
    fn a_span_this_posture_cannot_read_is_a_citation_and_is_reported() {
        let fixture = ResumeFixture::new("denied", "after\n");
        let view = fixture.view("/etc/hostname", crate::protocol::Origin::Attached);
        let restored = fixture.restore(&view, true);

        let code = &restored.context.turns[0].code_context;
        assert_eq!(code.len(), 1, "the turn keeps the fact that it read one");
        assert!(
            code[0]
                .text
                .starts_with("// not readable when this session was resumed:"),
            "{}",
            code[0].text,
        );
        assert_eq!(restored.unreadable.len(), 1);
        assert_eq!(restored.unreadable[0].turn, 1);
        assert_eq!(restored.unreadable[0].spec, "/etc/hostname");
        assert!(
            !restored.unreadable[0].why.is_empty(),
            "a denial that does not name the rule it broke is unreadable",
        );
    }

    /// A file that is simply gone is the other half, and has to read differently.
    ///
    /// *The policy refuses this path* and *this file is not there any more* mean
    /// opposite things about what to do next, so the citation carries the reason
    /// and not only the fact.
    #[test]
    fn a_span_whose_file_is_gone_says_that_rather_than_saying_denied() {
        let fixture = ResumeFixture::new("gone", "after\n");
        let view = fixture.view("vanished.txt", crate::protocol::Origin::Selected);
        let restored = fixture.restore(&view, true);

        assert_eq!(restored.unreadable.len(), 1);
        let why = restored.unreadable[0].why.clone();
        assert!(why.contains("vanished.txt"), "{why}");
        assert!(
            restored.context.turns[0].code_context[0]
                .text
                .contains(&why),
            "the citation carries the reason the report carries",
        );
    }

    /// A restored span is in the prompt, and is charged for.
    ///
    /// The turn's count is rebuilt over what it is carrying *now*, so a resumed
    /// session budgets against the window it actually has. Counting the prompt
    /// and the answer alone — which is all the count ever had to do while there
    /// was never any code — would make the first selection after a resume
    /// cheaper than the prompt it sends.
    #[test]
    fn a_restored_span_is_rendered_and_counted() {
        let fixture = ResumeFixture::new("counted", "after\n");
        let view = fixture.view("greeting.txt:1-1", crate::protocol::Origin::Selected);
        let mut restored = fixture.restore(&view, true);
        let bare = fixture.restore(&view, false);

        assert!(
            restored.context.turns[0].tokens > bare.context.turns[0].tokens,
            "a turn carrying a span costs more than the same turn carrying none",
        );

        let selection = restored.context.select(
            "and now?",
            &[],
            Budget::new(8192, 512, Eviction::Turn),
            &ApproximateCounter,
        );
        assert!(
            selection
                .messages
                .iter()
                .any(|message| message.content.contains("// greeting.txt:1-1")),
            "the restored span is in the prompt the resumed session sends",
        );
    }
}
