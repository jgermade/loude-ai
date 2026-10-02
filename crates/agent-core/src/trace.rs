//! The trace channel: what the agent did, as opposed to what it produced.
//!
//! Separate from [`crate::protocol`] on purpose. Trace messages exist to
//! explain the context manager — the exact prompt, how the token budget was
//! split — and a stdio consumer driving the agent should never have to carry
//! them. They travel on their own channel, only when `--trace` asks for it.
//!
//! The budget here is measured before the call, by our own counter. The
//! backend's own count arrives afterwards on `Ended.usage`, and the two are
//! meant to differ: the chat template is applied where we cannot see it. The
//! *difference* is the number worth watching — a stable gap is template
//! overhead, a moving one means the template changed.

use serde::{Deserialize, Serialize};

use crate::context::{Counter, TokenCounter};
use crate::protocol::TurnId;

/// One slice of the token budget. The names are not an enum: the context
/// manager will invent buckets faster than a wire enum can be revised, and a
/// UI that renders a stacked bar does not need to know them in advance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub name: String,
    pub tokens: u32,
}

impl Bucket {
    pub fn new(name: impl Into<String>, tokens: u32) -> Self {
        Self {
            name: name.into(),
            tokens,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceMessage {
    /// The exact string handed to the model, before any of it was generated.
    /// The panel that diffs this against the previous turn is the one that
    /// shows how much of the stable prefix survived.
    ///
    /// `tool_specs` is the request's own `tools` field under the native
    /// transport — counted in the `tools` bucket and absent from `text`, so
    /// without it a reader sees the count and never what was counted. `None`
    /// under the fenced transport, where the tools are in `text` already.
    Prompt {
        turn: TurnId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_specs: Option<String>,
    },
    /// How the context was spent, decided before the call rather than reported
    /// after it — so a cancelled turn has a budget too.
    ///
    /// `limit` is the model's window and `None` means unknown: then there is no
    /// headroom to draw, and saying so beats drawing a bar against nothing.
    /// `counter` is which counter produced these numbers, because two runs
    /// measured differently are not comparable and nothing else would say so.
    Budget {
        turn: TurnId,
        limit: Option<u32>,
        counter: Counter,
        buckets: Vec<Bucket>,
    },
    /// How much of this turn's prompt the previous turn's prompt already
    /// contained — the prompt cache's hit rate, as far as we can see it.
    ///
    /// A cache reuses a *prefix* and stops at the first difference, so the
    /// longest common prefix is the whole quantity: matching text after the
    /// divergence is reuse the cache never gets. Not emitted on the first turn
    /// of a session, where there is no previous prompt and "0%" would read as a
    /// measurement of a cold cache rather than the absence of one.
    PrefixReuse {
        turn: TurnId,
        /// Bytes shared with the previous turn's rendered prompt.
        shared_bytes: usize,
        /// Those bytes, by the same counter the budget was measured with. The
        /// shared prefix is tokenized as a substring, so its last token may not
        /// be one the model would emit at that boundary: an error of one token,
        /// the same order as the boundary error between buckets and as the chat
        /// template overhead, both of which are already accepted and reported.
        shared_tokens: u32,
        /// The whole rendered prompt, by that same counter, so the ratio is
        /// exact within one rendering.
        prompt_tokens: u32,
    },
    /// What the window took out of the prompt without taking the turn with it:
    /// the turns that gave up their spans, and what that saved.
    ///
    /// Here rather than on the protocol, which is where eviction goes, and the
    /// line between them is what happened to the *conversation*: an evicted
    /// turn is no longer in the session's window and cannot be answered from
    /// again, while a pruned one is still asked, still answered, still in the
    /// transcript. What it lost is code, which is a fact about the prompt — and
    /// facts about the prompt are what this channel is. See
    /// `RECORD/2026-09-08.prune-behind.completed.md`.
    Pruned {
        turn: TurnId,
        /// The turns that gave up spans, oldest first, named rather than
        /// counted for the reason an eviction tombstone names them: a reader
        /// months later cannot recover which without re-implementing the line.
        turns: Vec<TurnId>,
        /// The difference of two windows, by the counter below — not the sum of
        /// the spans, which under `Repeat::Once` is a saving the window does
        /// not get.
        tokens: u32,
        counter: Counter,
    },
    /// What rule A kept out of this render: spans a turn owns and did not send,
    /// because something else in this same prompt is already showing them.
    ///
    /// Beside [`Self::Pruned`] and for the same reason it is a trace — nothing
    /// about the conversation is different, every turn is still asked, answered
    /// and in the transcript, and what changed is the prompt.
    ///
    /// It is here because rule A became the **default** on 2026-09-19 and was
    /// the only one of the three window rules that said nothing: the number
    /// below is computed inside the render and was subtracted from the
    /// `history` bucket, and a difference is not a measurement of what made it.
    /// A run that collapsed forty spans and one that collapsed none left the
    /// same evidence — which is, one field along, what `record::FORMAT` 11 was
    /// bumped to fix for a `--prune-behind` run that pruned nothing. See
    /// `RECORD/2026-09-19.one-counting-surface.completed.md`.
    Repeated {
        turn: TurnId,
        /// The turns of the history that gave up spans, oldest first.
        turns: Vec<TurnId>,
        /// Whether the turn being asked gave one up to the history too. Named
        /// rather than listed for [`Self::Diverged::asking`]'s reason: it has no
        /// id when the render is made.
        asking: bool,
        /// Fragments unsent across the render.
        spans: usize,
        /// What they would have cost, summed. **The sum of the spans**, which
        /// is deliberately not what [`Self::Pruned::tokens`] is — see
        /// [`crate::context::Repeated::tokens`], which carries the argument.
        tokens: u32,
        counter: Counter,
    },
    /// A path the prompt just sent under more than one body: the same file,
    /// twice, with different contents and nothing saying which is true.
    ///
    /// The defect of `RECORD/2026-09-19.one-path-two-bodies.completed.md`.
    /// `code_context` is written once when a turn closes and never refreshed,
    /// while every turn re-reads its own spans, so a file edited between two
    /// turns goes out as two blocks under one `// path` header.
    ///
    /// A trace and not protocol, on the same argument [`Self::Pruned`] is a
    /// trace: nothing about the conversation is different — every turn is still
    /// asked, answered and in the transcript. What is different is what the
    /// prompt says, and facts about the prompt are what this channel is.
    ///
    /// Emitted per render and only where there is something to say, so a
    /// session that never edits a file it has quoted never sees one. It reports
    /// the defect; it does not repair it, and the record argues the repair and
    /// deliberately does not take it.
    Diverged {
        turn: TurnId,
        /// The span, as the fragment names it, line range included.
        path: String,
        /// The turns of the history that carried it, oldest first. The turn
        /// being asked is `turn` above and is named by `asking` rather than
        /// here, because it has no id until it is pushed.
        turns: Vec<TurnId>,
        /// Whether the turn being asked is one of the carriers — the case that
        /// matters most, because its bytes are the ones read this turn.
        asking: bool,
        /// Distinct bodies sent under this path. At least two.
        bodies: usize,
    },
    /// A path whose stale bytes this render replaced with the line that cites
    /// them, because a later turn read the same span and got different bytes.
    ///
    /// The repair of the line above, and the two are read as a pair: under
    /// `Repeat::Once` a render that supersedes is a render that no longer
    /// diverges, so a session carrying these and no `diverged` is the fix
    /// working rather than a session that never edited anything. The format
    /// number is what tells *that* apart from a stream written before the fix.
    ///
    /// A trace and not protocol, for [`Self::Diverged`]'s reason and more
    /// plainly: the conversation is untouched — every turn is still asked,
    /// answered and in the transcript — and what changed is which bytes the
    /// prompt carried for a path it had read twice.
    ///
    /// No `asking` beside [`Self::Diverged::asking`], and its absence is the
    /// invariant: the turn being asked holds the newest read of every span it
    /// carries and is never the turn superseded. See
    /// `RECORD/2026-09-20.the-newest-body-wins.completed.md`.
    Superseded {
        turn: TurnId,
        /// The span, as the fragment names it, line range included.
        path: String,
        /// The turns of the history whose body was replaced, oldest first.
        turns: Vec<TurnId>,
        /// Fragments replaced across the render.
        spans: usize,
        /// What their **bytes** would have cost, summed — the sum of the spans,
        /// as [`Self::Repeated::tokens`] is. Not the saving: a citation stands
        /// where each one stood and costs something of its own.
        tokens: u32,
        counter: Counter,
    },
    /// A model call *after* the first one of a turn: the tool-use round trip,
    /// or a schema retry.
    ///
    /// [`Self::Budget`] and [`Self::PrefixReuse`] describe the call that starts
    /// a turn. A turn that uses a tool makes more, each one carrying the
    /// previous result — and `usage.prompt_tokens` on `Ended` is summed over
    /// all of them, so on a tooled turn our count and the backend's count
    /// different things until these are added in. A call nothing measures is a
    /// cost nothing accounts for, and the difference was showing up in the
    /// panel as chat-template overhead.
    StepCall {
        turn: TurnId,
        /// Counts from 1 within the turn; the turn's own first call is
        /// already measured beside its budget, so this is only emitted for a
        /// second call — one after a tool ran (`step > 1`), or the one
        /// `SchemaRetry` spends on a drifted reply, which shares `step` with
        /// the attempt it retries. That second case is why this is *not*
        /// simply "`step > 1`": before it was measured explicitly, a
        /// retry's own call was silently absent from this chain, the gap
        /// `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md` names as "the
        /// tool-call probe's own instrument cannot see a retry".
        step: u32,
        /// The exact string this call handed to the model.
        text: String,
        prompt_tokens: u32,
        /// Against the call before it — the previous step, or the turn's own
        /// prompt — by the same counter and the same chain as everything else.
        shared_bytes: usize,
        shared_tokens: u32,
    },
}

impl TraceMessage {
    /// Measures one prompt against the one before it.
    ///
    /// What is compared is our own rendering, not the string the backend
    /// assembles from it — the chat template is applied where we cannot see it.
    /// It holds as a proxy because templates render message by message in
    /// order: if messages `0..k` are byte-identical across two calls, the
    /// templated prefix is identical too. What this measures is a property of
    /// the message sequence, which is what a context strategy changes.
    pub fn prefix_reuse(
        turn: TurnId,
        previous: &str,
        current: &str,
        counter: &dyn TokenCounter,
    ) -> Self {
        let shared_bytes = shared_prefix(previous, current);
        Self::PrefixReuse {
            turn,
            shared_bytes,
            shared_tokens: counter.count(&current[..shared_bytes]),
            prompt_tokens: counter.count(current),
        }
    }

    /// The same measurement for a call inside a turn, against the call before
    /// it. Same counter, same chain, so the numbers are comparable with the
    /// turn's own.
    pub fn step_call(
        turn: TurnId,
        step: u32,
        previous: &str,
        current: &str,
        counter: &dyn TokenCounter,
    ) -> Self {
        let shared_bytes = shared_prefix(previous, current);
        Self::StepCall {
            turn,
            step,
            shared_tokens: counter.count(&current[..shared_bytes]),
            prompt_tokens: counter.count(current),
            shared_bytes,
            text: current.to_string(),
        }
    }
}

/// The length in bytes of the longest common prefix, truncated to a character
/// boundary so the result can slice either string.
pub fn shared_prefix(previous: &str, current: &str) -> usize {
    let mut shared = previous
        .as_bytes()
        .iter()
        .zip(current.as_bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !current.is_char_boundary(shared) {
        shared -= 1;
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ApproximateCounter;

    #[test]
    fn a_budget_survives_the_wire() {
        let message = TraceMessage::Budget {
            turn: 1,
            limit: Some(8192),
            counter: Counter::Model { id: "qwen".into() },
            buckets: vec![Bucket::new("system", 120), Bucket::new("history", 640)],
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "budget");
        assert_eq!(json["buckets"][1]["name"], "history");
        assert_eq!(json["counter"]["kind"], "model");

        let back: TraceMessage = serde_json::from_value(json).unwrap();
        assert!(matches!(back, TraceMessage::Budget { buckets, .. } if buckets.len() == 2));
    }

    #[test]
    fn reuse_is_the_common_prefix_and_stops_at_the_first_difference() {
        let previous = "<|System|>\nfixed\n\n<|User|>\nold\n\n<|User|>\ntail";
        let current = "<|System|>\nfixed\n\n<|User|>\nnew\n\n<|User|>\ntail";

        let TraceMessage::PrefixReuse {
            shared_bytes,
            shared_tokens,
            prompt_tokens,
            ..
        } = TraceMessage::prefix_reuse(2, previous, current, &ApproximateCounter)
        else {
            panic!("prefix_reuse builds a PrefixReuse");
        };

        assert_eq!(
            &current[..shared_bytes],
            "<|System|>\nfixed\n\n<|User|>\n",
            "the shared trailing text is not reuse: a cache stops at the first difference",
        );
        assert!(shared_tokens < prompt_tokens);
    }

    #[test]
    fn a_prefix_that_diverges_mid_character_lands_on_a_boundary() {
        // Same first byte in UTF-8, different second: the byte-wise prefix ends
        // inside a character, and slicing there would panic.
        assert_eq!(shared_prefix("é", "è"), 0);
        assert_eq!(shared_prefix("añb", "añc"), 3, "the whole ñ is shared");
    }

    #[test]
    fn an_unknown_window_is_null_rather_than_zero() {
        let message = TraceMessage::Budget {
            turn: 1,
            limit: None,
            counter: Counter::Approximate,
            buckets: vec![Bucket::new("prompt", 12)],
        };
        let json = serde_json::to_value(&message).unwrap();
        assert!(json["limit"].is_null(), "0 would plot as a window of zero");
        assert_eq!(json["counter"]["kind"], "approximate");
    }
}
