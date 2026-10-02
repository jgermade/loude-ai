//! The agent protocol: one message schema, several transports.
//!
//! These types are carried unchanged over stdio (the VSCode bridge), over a
//! WebSocket (the debug UI), and into the record format. Adding a field that
//! only one of them understands is how the schema stops being one schema.
//!
//! **Debug data does not live here.** Prompts, token budgets and anything else
//! that exists to explain the agent rather than drive it belong in
//! [`crate::trace`], on its own channel behind `--trace`, so a stdio consumer
//! never has to carry them.
//!
//! Every variant below was written after watching `run_turn` produce it —
//! see `RECORD/2026-08-26.walking-skeleton.completed.md`.

use serde::{Deserialize, Serialize};

use crate::approval::Signature;
use crate::backend::Usage;
use crate::context::{Counter, Eviction};
use crate::job::{ApprovedBy, ClosedBy, JobId, Plan, PlanSource, Replaced};
use crate::sandbox::Verdict;
use crate::tools::ToolStep;
use crate::turn::{EndReason, TurnEvent};

/// Bumped when a change would break an older client. `Hello` carries it so a
/// mismatch is a message rather than a mystery.
///
/// **v1 was frozen** once every message in it had been watched being sent and
/// answered, the task lifecycle included, with the rule that a change an older
/// reader could not make sense of bumps this and [`crate::record::FORMAT`] with
/// it.
///
/// **2 is that rule being used**, for [`ServerMessage::Refused`]: an unknown
/// `type` in a tagged enum is a parse error rather than a line to skip, so a new
/// variant is exactly the change the rule names. See
/// `RECORD/2026-08-30.a-refusal-is-a-message.completed.md`.
///
/// **3 is the same rule again**, for [`ServerMessage::Evicted`]: what leaves the
/// window is a thing that happened to the conversation, not a debug reading, so
/// it is here rather than on the trace channel — and a client that could not
/// parse it would be watching a transcript whose turns are silently no longer
/// in the prompt. See `RECORD/2026-08-31.eviction-tombstones.completed.md`.
///
/// **4 uncrosses terminology**: the outer unit of work and approval is renamed to
/// **Job**, reserving **Task** for the fine-grained checklist items emitted by models.
///
/// **5 is the wire learning to say what it speaks**, which is federation's first
/// prerequisite: [`ClientMessage::Hello`] is a new client variant, and
/// [`Refusal`] gains `version` and `signature` — new values of a tagged enum,
/// which is the same rule as 2, 3 and 4. This is also the last bump an older
/// client learns about by failing to parse: from here a client says its version
/// and is refused out loud. See
/// `RECORD/2026-09-04.signed-approvals.completed.md`.
///
/// **6 was a session arriving from another host, and is not a version anything
/// speaks.** `imported` was built and removed the same day: a session belongs to
/// the host that made it, so there is no border for a message to describe. The
/// bump was un-made rather than left standing — a number whose whole job is to
/// tell two peers what they can parse must not carry a variant that no longer
/// exists. See `RECORD/2026-09-04.sessions-stay-home.completed.md`.
///
/// **6, taken this time: [`ServerMessage::Grounded`]**, which says what a turn
/// was asked with by reference, so that a resume can read it again instead of
/// losing it. Same rule as 2, 3 and 4 — a new variant of a tagged enum — and
/// the number is the one the paragraph above un-made, free for exactly the
/// reason `record::FORMAT` reused its own un-made 8: no peer ever spoke it, so
/// there is nobody to disagree with about what it meant. It is the first bump
/// since 5, which is where a client started saying its version and being
/// refused out loud rather than failing to parse — so this is also the first
/// one an older client learns about by being told. See
/// `RECORD/2026-09-19.fragments-by-reference.completed.md`.
///
/// **7 is the session becoming an alternation**, and it is the largest bump
/// here since 4 renamed the unit of work: `draft_opened`, `plan_proposed` and
/// `plan_declined` are three things a client has to see to draw the gate and
/// none of them existed, `job_approved` gains the draft it closed, and the gate
/// stops naming a job in either direction — `approve_plan` and `decline_plan`
/// address the one plan on the table, because a proposal is offered inside a
/// draft and an id is what approval hands out. `job_proposed` and
/// `job_rejected` stay parseable and are never written again: they describe a
/// job that under this shape is not one, and a recording that contains them is
/// a recording from before it. Same rule as 2, 3 and 4 for the new variants,
/// and the first bump that *retires* any. See
/// `RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md`.
///
/// **8 is the rung the alternation made common**: a draft closed because a plan
/// was approved out of it now says `approval` on `job_closed.by`, where it had
/// been indistinguishable from a person folding it. Not a new variant of a
/// message but a new **value** of one's field, which is the same break for a
/// reader — `ClosedBy` is a plain enum and an unknown value is a parse error,
/// exactly as an unknown `type` is. The first bump taken for a field rather
/// than for a line. See
/// `RECORD/2026-09-21.the-alternation-on-the-page.completed.md`.
///
/// **9 is a person answering one call**: [`ServerMessage::CallHeld`] when the
/// floor refused a write the policy file would allow, and
/// [`ClientMessage::AnswerCall`] for *allow once* or *deny*. New variants of
/// both tagged enums, the rule 2 named; and `Authority` gains `person`, which a
/// reader of a verdict's stamp meets as a new value. `tool_result.asked` is
/// additive. See `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
pub const VERSION: u32 = 9;

/// Turns are numbered per session, in order, starting at 1.
pub type TurnId = u64;

/// Why the server did not do what was asked. Small on purpose: a client
/// branches on this and shows [`ServerMessage::Refused::detail`] to a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// A turn is running. One at a time until sessions exist.
    Busy,
    /// A proposal is waiting on a person. Nothing runs behind the gate.
    Pending,
    /// No call is held at the turn and step an [`ClientMessage::AnswerCall`]
    /// named: it was answered already, or its turn ended first.
    Call,
    /// The job named is not in a state where the ask applies — approved
    /// twice, closed while nothing was open, reopened when it was never closed.
    #[serde(alias = "task")]
    Job,
    /// Part of what was asked for is not granted by the policy file, which is
    /// the outer bound nobody may widen past.
    NotGranted,
    /// The client speaks a protocol or record format this host does not. Sent
    /// in answer to [`ClientMessage::Hello`], in either direction of mismatch:
    /// a newer client is the case this host cannot parse and an older one is
    /// the case it cannot, and neither side can repair it by guessing.
    Version,
    /// The approval carries no signature where one is required, names a key
    /// this host does not know, or does not verify against the grant it
    /// carries. One variant and not three: a client branches on this and shows
    /// `detail` to a person.
    Signature,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// What the client speaks. Optional on loopback and over stdio, where the
    /// peer is this machine's own browser or the process that spawned us;
    /// **required as the first message** on a port that asks for a bearer
    /// token, which is every port off loopback. A mismatch is answered with
    /// [`Refusal::Version`] and the connection closed, rather than by
    /// misreading the next message.
    Hello {
        protocol: u32,
        /// The record format the client can read, so a client that mirrors or
        /// replays a session is refused for the same reason at the same point.
        #[serde(default)]
        format: u32,
    },
    /// Start a turn.
    Prompt { text: String },
    /// Stop the turn in flight. Cancelling when nothing is running is not an
    /// error — a client that raced the end of a turn did nothing wrong.
    Cancel,
    /// Approve the plan on the table. Nothing has run under it until this
    /// arrives: the prompt that caused the proposal is held, unrun, until it
    /// does.
    ///
    /// **It names no job, and that is the change.** A proposal is a thing
    /// offered inside a draft, not a job in a state, and an id is what approval
    /// hands out — so there is nothing to name until this message is answered.
    /// At most one plan is ever on the table, which is what makes the address
    /// unnecessary rather than merely absent. The aliases keep a frame from a
    /// client that still writes `approve_job` parsing; its `job` field is
    /// ignored, because there was never a job for it to have been right about.
    #[serde(alias = "approve_task", alias = "approve_job")]
    ApprovePlan {
        /// **The id this approval is about to hand out** — not one that exists.
        ///
        /// The one thing the gate still names, and only because a signature has
        /// to be bound to something: an [`crate::approval::Approval`] covers a
        /// grant *in a session for a job*, and one that could be replayed
        /// against a different job is bound to nothing. A client reads it off
        /// the session as *one past the last job* and the server refuses a
        /// mismatch, so a stale client cannot sign for a job the session is not
        /// about to open.
        ///
        /// `None` on an unsigned approval, where there is nothing to bind and
        /// nothing to check. It is never an address: no lookup is ever done
        /// with it, because the thing being approved is not a job yet.
        #[serde(default, alias = "task")]
        job: Option<JobId>,
        /// What the job may read, added to what the model declared.
        #[serde(default)]
        files: Vec<String>,
        /// What it may also change. Separate from `files` for the same reason
        /// the plan separates them: a grant that cannot say *read* cannot be
        /// held to reading.
        #[serde(default)]
        writes: Vec<String>,
        #[serde(default)]
        commands: Vec<String>,
        /// What would close this job without anyone present: a command line
        /// whose exit code of 0 folds it.
        #[serde(default)]
        closes_on: Option<String>,
        /// Whether to grant network access to this job. If `None`, preserves
        /// whatever the model's plan requested (or `false` by default).
        #[serde(default)]
        network: Option<bool>,
        /// How hard the kernel is asked to hold this job's children. `None`
        /// leaves the plan's own answer, which is usually the session's.
        ///
        /// Additive and `Option`, so this is a new *field* rather than a new
        /// variant and neither the protocol nor the record format moves. A
        /// person may tighten here and is refused if they try to loosen, which
        /// is [`crate::job::Plan::unmet`]'s business and not this type's.
        #[serde(default)]
        enforcement: Option<crate::sandbox::Enforcement>,
        /// Optional egress domain allowlist for this job.
        #[serde(default)]
        egress: Option<Vec<String>>,
        /// Ed25519 over the grant above, bound to the session it belongs to.
        /// Required when `[approvals] required` says so, verified whenever it
        /// is present. See [`crate::approval`].
        #[serde(default)]
        signature: Option<Signature>,
    },
    /// Refuse the plan on the table. *Decline* and *more changes* are the same
    /// answer — **not yet** — and neither ends anything.
    ///
    /// **The held prompt now runs** rather than being dropped, which is the one
    /// behaviour in item 22 a person can observe changing. It is §seventh's own
    /// sentence — *nothing closes, nothing folds, the conversation continues* —
    /// and the turn it runs is what opens the draft the conversation continues
    /// in. Names no job for [`Self::ApprovePlan`]'s reason.
    #[serde(alias = "reject_task", alias = "reject_job")]
    DeclinePlan,
    /// The answer to a [`ServerMessage::CallHeld`]: `allow` runs that call once
    /// under the policy file, and only that call; otherwise the model is told a
    /// person refused it. `turn` and `step` name the call, so an answer that
    /// raced the end of its turn is refused rather than applied to the next
    /// held call.
    AnswerCall {
        turn: TurnId,
        step: u32,
        allow: bool,
    },
    /// Close it: from here its turns are sent as their summary.
    #[serde(alias = "close_task")]
    CloseJob {
        #[serde(alias = "task")]
        job: JobId,
    },
    /// Ask for a plan over the draft as it stands.
    ///
    /// The explicit door, beside the model's own suggestion. It runs the
    /// planning call as a **compaction of the open draft** — the refinement is
    /// the draft, and the plan is what survives it — so unlike every planning
    /// call before item 22 it reads a window somebody has already filled rather
    /// than a prompt nobody has looked at yet.
    RequestPlan,
    /// Unfold it. Not an undo: nothing was deleted to recover.
    #[serde(alias = "reopen_task")]
    ReopenJob {
        #[serde(alias = "task")]
        job: JobId,
    },
}

/// Where a span in a turn's prompt came from.
///
/// The distinction exists in the tree and until now could be seen from
/// nowhere: `chat` builds a turn's code out of the fragments a person typed
/// and then the spans relevance selection chose, in that order, into one
/// `Vec<Fragment>` that nothing afterwards can take apart. `serve` has no
/// `--fragment` at all, so every span in a session the store holds is
/// `Selected`. Both are restored across a resume, for the reason
/// `RECORD/2026-09-19.fragments-by-reference.completed.md` §Two things gives;
/// this is here so that the next decision about them is a decision rather than
/// a rediscovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// A person typed it — `--fragment`, or `## fragment:` in a script.
    Attached,
    /// Relevance selection chose it, scored against this turn's own text.
    Selected,
}

/// One span a turn was asked with, **by reference**.
///
/// The spec and not the bytes, and that is the whole of the decision: bytes in
/// the store come back without being re-read, so a session resumed under a
/// narrower posture would get back a file it is no longer allowed to open.
/// The spec is enough to read it again through whatever sandbox the resume is
/// under, and reading it again is the only thing that can be honest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grounding {
    /// The spec as it was written — `src/lib.rs:12-40` — which is the string
    /// [`crate::context::Fragment::path`] carries.
    pub spec: String,
    pub origin: Origin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent once on connect. A client that attaches mid-turn learns what it
    /// missed from `turn`, rather than inferring it from the first token.
    Hello {
        protocol: u32,
        backend: String,
        model: String,
        turn: Option<TurnId>,
        /// What this session is called, so an approval can be signed against
        /// it. Without it a signature captured here replays against any other
        /// host that numbers its jobs the same way. `None` in a recording made
        /// before signatures existed.
        #[serde(default)]
        session: Option<String>,
    },
    /// Carries the prompt, because a second client (and the record file) never
    /// saw the `ClientMessage` that started the turn.
    TurnStarted {
        turn: TurnId,
        prompt: String,
        /// The job it was asked inside, when there is one.
        #[serde(default, alias = "task")]
        job: Option<JobId>,
    },
    /// What this turn was asked with, beside what was asked: the spans fused
    /// into its user message, by reference.
    ///
    /// **Here and not on the trace channel**, which is the opposite of where
    /// `pruned` and `diverged` went, and the line is between the reference and
    /// the bytes: that a turn was asked with a file in front of it is a thing
    /// that happened, and for an `Attached` span it is what the person typed.
    /// The bytes are the prompt fact, they are already on the trace channel
    /// inside `prompt`, and they are what this refuses to carry. Without it a
    /// resumed session comes back with turns that never had code — the one
    /// place in this system where something was lost rather than merely not
    /// sent. See `RECORD/2026-09-19.fragments-by-reference.completed.md`.
    ///
    /// Absent on a turn that carried none, and on every turn of every stream
    /// written before protocol 6.
    Grounded {
        turn: TurnId,
        spans: Vec<Grounding>,
    },
    /// A draft opened, because a turn landed where nothing was open.
    ///
    /// The session's own shape on the wire: every turn belongs to a job, and
    /// this is the message that makes that true from the first one. A client
    /// draws the draft from here and attributes the turn that caused it.
    DraftOpened {
        job: JobId,
        /// The turn that opened it, which is the only way a job ever opens.
        ///
        /// Carried rather than left for a client to infer, because it is the
        /// only place that attribution exists: `turn_started` says which job a
        /// turn is *in*, and a turn that opens a draft is in one that did not
        /// exist when it started. Without this the first turn of every session
        /// would read as belonging to nothing, which is the exact claim item 22
        /// retired.
        turn: TurnId,
        /// What the user asked, as they asked it — a draft's objective is known
        /// and its plan is not, which is the whole of what a draft is.
        objective: String,
    },
    /// A plan is on the table, from the model's own suggestion or from
    /// `request_plan`. Nobody has approved anything.
    ///
    /// **It names no job.** A proposal is offered inside the open draft and an
    /// id is what approval hands out, so there is nothing to name yet — and a
    /// plan that is declined never acquires one, which is what retired
    /// `JobProposed` below. At most one is on the table at a time.
    PlanProposed {
        objective: String,
        plan: Plan,
        #[serde(default)]
        source: Option<PlanSource>,
    },
    /// The plan on the table was turned down, or a prompt arrived instead of an
    /// answer — *decline* and *more changes* are the same answer. Nothing
    /// closed and nothing folded; the draft it was offered inside is still
    /// open. Pairs with the [`Self::PlanProposed`] before it, by order.
    PlanDeclined,
    /// A piece of work, with the plan that is about to be approved or refused.
    /// Nothing runs between this and `JobApproved`.
    ///
    /// **Historical: never written at protocol 7 or above.** It survives so a
    /// recording made before the alternation still replays, and the format
    /// number is what tells a reader which shape they are looking at. Under the
    /// alternation the thing it described is not a job — see
    /// [`Self::PlanProposed`].
    #[serde(alias = "task_proposed")]
    JobProposed {
        #[serde(alias = "task")]
        job: JobId,
        objective: String,
        plan: Plan,
        #[serde(default)]
        source: Option<PlanSource>,
    },
    /// Approved, with the plan as approved rather than as proposed: it is what
    /// the job's sandbox is built from.
    #[serde(alias = "task_approved")]
    JobApproved {
        #[serde(alias = "task")]
        job: JobId,
        /// The draft this approval closed on its way in, when one was open to
        /// close. Approving *is* closing, so the two halves of the transition
        /// travel together and a client never has to infer one from the other.
        ///
        /// `None` when no draft had opened — no turn had landed in one — and in
        /// every recording written before protocol 7, where an approval opened
        /// nothing and closed nothing. The `JobClosed` for the draft is sent
        /// before this and carries the fold itself.
        #[serde(default)]
        from: Option<JobId>,
        /// What the job is called. The approved plan's own objective, which
        /// under the alternation is what the draft compacted to rather than
        /// what a held prompt asked for.
        #[serde(default)]
        objective: String,
        #[serde(default)]
        plan: Plan,
        /// Which authority approved it. Absent in a recording written before
        /// signatures existed, which reads as [`ApprovedBy::Operator`].
        #[serde(default)]
        approved_by: Option<ApprovedBy>,
    },
    /// The one rewrite in an otherwise write-once session: from here on the
    /// job's turns are sent as this summary.
    #[serde(alias = "task_closed")]
    JobClosed {
        #[serde(alias = "task")]
        job: JobId,
        summary: String,
        #[serde(default)]
        by: Option<ClosedBy>,
        /// What the fold took out of the prompt and what it was worth there,
        /// counted at the close with the counter that counted the summary.
        /// Absent in a stream written before format 9 — which is not zero. See
        /// `RECORD/2026-09-08.what-a-fold-writes-down.completed.md`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaced: Option<Replaced>,
    },
    /// What left the window, and stays out.
    Evicted {
        turn: TurnId,
        turns: Vec<TurnId>,
        tokens: u32,
        counter: Counter,
        policy: Eviction,
    },
    /// The fold stops applying. Not an undo — nothing was deleted.
    #[serde(alias = "task_reopened")]
    JobReopened {
        #[serde(alias = "task")]
        job: JobId,
    },
    /// The plan was put up and turned down.
    ///
    /// **Historical: never written at protocol 7 or above**, for
    /// [`Self::JobProposed`]'s reason and replaced by [`Self::PlanDeclined`].
    #[serde(alias = "task_rejected")]
    JobRejected {
        #[serde(alias = "task")]
        job: JobId,
    },
    Token {
        turn: TurnId,
        text: String,
    },
    /// `usage` is absent on a cancelled turn: the counts arrive on the
    /// backend's final line, which cancelling means never reading. Reporting
    /// zeros instead would be a lie the budget panel would plot.
    Ended {
        turn: TurnId,
        reason: EndReason,
        usage: Option<Usage>,
    },
    Failed {
        turn: TurnId,
        message: String,
    },
    /// The server did not do what a client asked, and why.
    ///
    /// Not a failure and not a turn: the session is in a state where the ask
    /// does not apply. Before this existed the server simply returned, and a
    /// client could not tell a refusal from a message that never arrived — so
    /// the UI guessed, by disabling its own composer, which is not a permission
    /// model and not an interface either.
    ///
    /// It travels on the protocol rather than the trace channel because it
    /// *drives* a client: it is the answer to why nothing happened.
    Refused {
        /// The `type` of the client message being refused.
        request: String,
        reason: Refusal,
        /// The same thing in words, for a person to read.
        detail: String,
    },
    /// A tool the model asked for, before it was checked. `step` counts from 1
    /// within the turn.
    ToolCall {
        turn: TurnId,
        step: u32,
        name: String,
        arguments: serde_json::Value,
    },
    /// The call `step` was refused by a draft's floor and the policy file would
    /// allow it, so the turn is waiting on a person rather than handing the
    /// refusal to the model. `refused` is the floor's rule, so the page can say
    /// what it is being asked to override. Answered by
    /// [`ClientMessage::AnswerCall`]; the `tool_result` that follows carries
    /// the answer in `asked`.
    CallHeld {
        turn: TurnId,
        step: u32,
        name: String,
        arguments: serde_json::Value,
        refused: String,
    },
    /// What it did. The verdict travels with the result because "the agent ran
    /// a command" and "the kernel held the command it ran" are different facts
    /// and only one of them is worth trusting.
    ToolResult {
        turn: TurnId,
        step: u32,
        name: String,
        verdict: Verdict,
        error: Option<String>,
        /// The result as the model received it — the same bytes that went into
        /// the history, so a recording can show what the model was told.
        output: String,
        truncated: bool,
        duration_ms: u64,
        /// What a subprocess did, for the tool where that is a fact: the exit
        /// code, the signal, the two streams unmixed. Absent for every
        /// in-process tool and in every recording made before it existed, which
        /// is why it is additive rather than a format bump — an older reader
        /// ignores a field it does not know, and this variant is not new.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<crate::tools::CommandResult>,
        /// A person's answer, when the floor refused this call and somebody was
        /// asked: `true` allowed it once, `false` refused it. Absent when
        /// nobody was asked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asked: Option<bool>,
    },
}

impl ServerMessage {
    /// The one conversion at the core/wire boundary.
    ///
    /// `None` for an event that is not a protocol message. There is one:
    /// [`TurnEvent::ModelCall`] explains the agent rather than driving it, so it
    /// belongs on the trace channel and a stdio consumer never sees it. That is
    /// the trace/protocol split stopping being a convention and becoming a
    /// type — an internal event that provably cannot reach the wire.
    pub fn from_turn_event(turn: TurnId, event: TurnEvent) -> Option<Self> {
        Some(match event {
            TurnEvent::ModelCall { .. } => return None,
            TurnEvent::ConstraintRefused { .. } => return None,
            TurnEvent::Token(text) => Self::Token { turn, text },
            TurnEvent::Ended { reason, usage } => Self::Ended {
                turn,
                reason,
                usage,
            },
            TurnEvent::Failed(message) => Self::Failed { turn, message },
            TurnEvent::ToolCall { step, call } => Self::ToolCall {
                turn,
                step,
                name: call.name,
                arguments: call.arguments,
            },
            TurnEvent::CallHeld {
                step,
                call,
                refused,
            } => Self::CallHeld {
                turn,
                step,
                name: call.name,
                arguments: call.arguments,
                refused,
            },
            TurnEvent::ToolResult { step, outcome } => {
                let ToolStep {
                    call,
                    outcome,
                    duration_ms,
                    asked,
                    ..
                } = *outcome;
                Self::ToolResult {
                    turn,
                    step,
                    name: call.name,
                    verdict: outcome.verdict,
                    error: outcome.error,
                    output: outcome.output,
                    truncated: outcome.truncated,
                    duration_ms,
                    command: outcome.command,
                    asked,
                }
            }
        })
    }

    /// Which turn this is about, if any.
    pub fn turn(&self) -> Option<TurnId> {
        match self {
            Self::Hello { turn, .. } => *turn,
            Self::TurnStarted { turn, .. }
            | Self::Token { turn, .. }
            | Self::Ended { turn, .. }
            | Self::Failed { turn, .. }
            | Self::ToolCall { turn, .. }
            | Self::CallHeld { turn, .. }
            | Self::ToolResult { turn, .. }
            // The turn that cut, not the turns that left: this is a thing the
            // selection for `turn` did.
            | Self::Evicted { turn, .. }
            // And the turn that was grounded, which is the same shape: what
            // this turn was asked with.
            | Self::Grounded { turn, .. } => Some(*turn),
            // A task spans turns and its lifecycle happens between them, and a
            // refusal is about the ask rather than about a turn — three of the
            // four happen when there is no turn to name.
            Self::DraftOpened { .. }
            | Self::PlanProposed { .. }
            | Self::PlanDeclined
            | Self::JobProposed { .. }
            | Self::JobApproved { .. }
            | Self::JobClosed { .. }
            | Self::JobReopened { .. }
            | Self::JobRejected { .. }
            | Self::Refused { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(message: &ServerMessage) -> serde_json::Value {
        let json = serde_json::to_value(message).unwrap();
        let back: ServerMessage = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(format!("{back:?}"), format!("{message:?}"));
        json
    }

    /// The alias is for a client that has not been rewritten, and it is a
    /// second *spelling*, not a second field.
    ///
    /// A frame naming both is `job` twice, which serde refuses and the server
    /// drops — silently, because an unparseable frame must not take the socket
    /// down with it. That is how every approval this repository's own page sent
    /// was discarded between the rename and the day somebody clicked Approve in
    /// a browser and watched the gate stay shut.
    #[test]
    fn a_gate_frame_names_the_job_once() {
        let approve = |body: &str| serde_json::from_str::<ClientMessage>(body);

        assert!(
            approve(r#"{"type":"approve_job","job":1}"#).is_ok(),
            "the spelling every current client sends",
        );
        assert!(
            approve(r#"{"type":"approve_task","task":1}"#).is_ok(),
            "and the one written before the rename, which the alias exists for",
        );
        let both = approve(r#"{"type":"approve_job","job":1,"task":1}"#)
            .expect_err("naming the same field twice is not a frame");
        assert!(
            both.to_string().contains("duplicate field"),
            "the error is what it is: {both}",
        );
    }

    #[test]
    fn a_token_is_tagged_by_type() {
        let json = roundtrip(&ServerMessage::Token {
            turn: 1,
            text: "hola".into(),
        });
        assert_eq!(json["type"], "token");
        assert_eq!(json["text"], "hola");
    }

    #[test]
    fn an_eviction_names_the_turns_that_left_and_who_counted_them() {
        let json = roundtrip(&ServerMessage::Evicted {
            turn: 12,
            turns: vec![1, 2, 3],
            tokens: 843,
            counter: Counter::Model {
                id: "qwen2.5-coder:7b".into(),
            },
            policy: Eviction::Block { low_water: 0.5 },
        });
        assert_eq!(json["type"], "evicted");
        assert_eq!(json["turn"], 12, "the turn whose selection cut");
        assert_eq!(json["turns"], serde_json::json!([1, 2, 3]));
        assert_eq!(json["counter"]["id"], "qwen2.5-coder:7b");
        assert_eq!(
            json["policy"]["policy"], "block",
            "which cut it was, beside how much it took",
        );
        assert_eq!(
            ServerMessage::Evicted {
                turn: 12,
                turns: vec![1],
                tokens: 0,
                counter: Counter::Approximate,
                policy: Eviction::Turn,
            }
            .turn(),
            Some(12),
            "it is a thing the cutting turn did, not a message about the turns that left",
        );
    }

    #[test]
    fn a_cancelled_end_carries_a_null_usage_rather_than_zeros() {
        let json = roundtrip(&ServerMessage::Ended {
            turn: 1,
            reason: EndReason::Cancelled,
            usage: None,
        });
        assert_eq!(json["reason"], "cancelled");
        assert!(json["usage"].is_null());
    }

    #[test]
    fn a_model_call_never_reaches_the_wire() {
        // It explains the agent rather than driving it, so it belongs on the
        // trace channel and a stdio consumer never has to carry it.
        assert!(
            ServerMessage::from_turn_event(
                1,
                TurnEvent::ModelCall {
                    step: 2,
                    messages: Vec::new(),
                    retry: false,
                }
            )
            .is_none()
        );
    }

    #[test]
    fn a_client_prompt_parses_from_the_wire() {
        let parsed: ClientMessage =
            serde_json::from_str(r#"{"type":"prompt","text":"hola"}"#).unwrap();
        assert!(matches!(parsed, ClientMessage::Prompt { text } if text == "hola"));
    }

    #[test]
    fn a_job_message_is_about_a_job_rather_than_a_turn() {
        let json = roundtrip(&ServerMessage::JobProposed {
            job: 2,
            objective: "add a --dry-run flag".into(),
            plan: Plan {
                tasks: vec!["read the CLI".into()],
                files: vec!["crates/luu/src/lib.rs".into()],
                writes: vec![],
                commands: vec!["cargo".into()],
                closes_on: None,
                network: false,
                ..Plan::default()
            },
            source: Some(PlanSource::Model),
        });
        assert_eq!(json["type"], "job_proposed");
        assert_eq!(json["plan"]["files"][0], "crates/luu/src/lib.rs");
        assert_eq!(
            json["source"], "model",
            "who wrote the plan, which an empty one cannot be asked",
        );

        // An older recording has no `source`, and it stays unknown rather than
        // becoming a claim the recording never made.
        let older: ServerMessage =
            serde_json::from_str(r#"{"type":"task_proposed","task":1,"objective":"x","plan":{}}"#)
                .expect("an added optional field is not a parse error");
        assert!(matches!(
            older,
            ServerMessage::JobProposed { source: None, .. }
        ));
        assert_eq!(
            ServerMessage::JobApproved {
                job: 2,
                from: Some(1),
                objective: "add a --dry-run flag".into(),
                plan: Plan::default(),
                approved_by: None,
            }
            .turn(),
            None,
            "a job spans turns; pinning it to one would be a guess",
        );
    }

    #[test]
    fn the_client_half_of_the_lifecycle_parses_from_the_wire() {
        for (text, expected) in [
            (r#"{"type":"approve_plan"}"#, "ApprovePlan"),
            (r#"{"type":"decline_plan"}"#, "DeclinePlan"),
            (r#"{"type":"request_plan"}"#, "RequestPlan"),
            (r#"{"type":"close_job","job":2}"#, "CloseJob"),
            (r#"{"type":"reopen_job","job":2}"#, "ReopenJob"),
            // A client still writing the pre-alternation spellings reaches the
            // same handler. Its `job` is read as the id the approval will hand
            // out, which is the only thing that field can mean now — and a
            // stale one is refused by the server rather than followed.
            (r#"{"type":"approve_job","job":2}"#, "ApprovePlan"),
            (r#"{"type":"reject_job","job":2}"#, "DeclinePlan"),
            (r#"{"type":"approve_task","task":2}"#, "ApprovePlan"),
            (r#"{"type":"reject_task","task":2}"#, "DeclinePlan"),
            (r#"{"type":"close_task","task":2}"#, "CloseJob"),
            (r#"{"type":"reopen_task","task":2}"#, "ReopenJob"),
        ] {
            let parsed: ClientMessage = serde_json::from_str(text).unwrap();
            assert!(
                format!("{parsed:?}").starts_with(expected),
                "{text} parsed as {parsed:?}",
            );
        }
    }

    #[test]
    fn the_client_says_what_it_speaks_and_an_approval_can_carry_a_signature() {
        let hello: ClientMessage =
            serde_json::from_str(r#"{"type":"hello","protocol":5,"format":7}"#).unwrap();
        assert!(matches!(
            hello,
            ClientMessage::Hello {
                protocol: 5,
                format: 7
            }
        ));

        // A client that only drives the wire and never reads a recording says
        // nothing about the format, and that is not a mismatch.
        let bare: ClientMessage = serde_json::from_str(r#"{"type":"hello","protocol":5}"#).unwrap();
        assert!(matches!(bare, ClientMessage::Hello { format: 0, .. }));

        let signed: ClientMessage = serde_json::from_str(
            r#"{"type":"approve_plan","job":2,"signature":{"by":"jgermade","sig":"ab"}}"#,
        )
        .unwrap();
        let ClientMessage::ApprovePlan { job, signature, .. } = signed else {
            panic!("an approval");
        };
        assert_eq!(signature.expect("the signature").by, "jgermade");
        assert_eq!(
            job,
            Some(2),
            "a signed approval names the id it will hand out, because that is \
             what the signature is bound to",
        );

        // And an approval from before signatures existed is still an approval.
        let unsigned: ClientMessage =
            serde_json::from_str(r#"{"type":"approve_job","job":2}"#).unwrap();
        assert!(matches!(
            unsigned,
            ClientMessage::ApprovePlan {
                signature: None,
                ..
            }
        ));

        // And an unsigned one need not name a job at all: there is nothing to
        // bind, and the id it would name does not exist yet.
        let bare: ClientMessage = serde_json::from_str(r#"{"type":"approve_plan"}"#).unwrap();
        assert!(matches!(bare, ClientMessage::ApprovePlan { job: None, .. }));
    }

    #[test]
    fn an_approval_recorded_before_signatures_existed_still_parses() {
        let parsed: ServerMessage =
            serde_json::from_str(r#"{"type":"task_approved","task":1,"plan":{}}"#).unwrap();
        assert!(
            matches!(
                parsed,
                ServerMessage::JobApproved {
                    approved_by: None,
                    ..
                }
            ),
            "absent is not a claim about who approved; the reader decides it was the operator",
        );
    }

    #[test]
    fn a_turn_recorded_before_tasks_existed_still_parses() {
        let parsed: ServerMessage =
            serde_json::from_str(r#"{"type":"turn_started","turn":1,"prompt":"hola"}"#).unwrap();
        assert!(matches!(
            parsed,
            ServerMessage::TurnStarted { job: None, .. }
        ));
    }

    #[test]
    fn turn_events_convert_without_losing_the_reason() {
        let event = TurnEvent::Ended {
            reason: EndReason::Length,
            usage: None,
        };
        let message = ServerMessage::from_turn_event(7, event).unwrap();
        assert_eq!(message.turn(), Some(7));
        assert!(matches!(
            message,
            ServerMessage::Ended {
                reason: EndReason::Length,
                ..
            }
        ));
    }
}
