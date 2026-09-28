//! The edit-and-re-read corpus's harness: proved without a model.
//!
//! `tool_call_probe.rs` proves a scorer, a corpus and the mock compose so that
//! running the corpus against a model later is the only new variable. This is
//! the same shape one neighbourhood along, for
//! [`scripts/tasks/edit-reread.txt`](../../../scripts/tasks/edit-reread.txt) —
//! thirteen turns that read four files, edit three of them and ask about two
//! of those again — and it runs the **real binary** against the **real
//! sandbox**, because `code_context` going stale is a property of a session
//! rather than of a reply.
//!
//! What it proves is that the corpus produces what it was built to produce:
//! **eight of the session's thirteen prompts** carry two bodies of
//! `src/greeting.rs`, off two edits, and **nothing at all** is said about the
//! three files that are there to stay quiet.
//!
//! Those eight are now the `--no-repeat-once` arm's, and the default's eight
//! are the same renders *repaired*: one body goes out and the stale ones are
//! the line that cites them. The two arms are read together on purpose — a
//! session with no divergences and no supersessions never edited a file it had
//! quoted, and telling that apart from a session where the fix did its job is
//! what the pair is for. See
//! `RECORD/2026-09-20.the-newest-body-wins.completed.md`. The two silences are the point
//! as much as the noise is — a corpus whose control also fires is a corpus that
//! measures nothing, and the two blind spots
//! [`one-path-two-bodies`](../../../RECORD/2026-09-19.one-path-two-bodies.completed.md)
//! §Still open names are only named there. Here they are reproducible.
//!
//! This is *not* a claim about a model. The edits are scripted, so the number
//! it reports is a fact about the corpus and not about how often a model edits
//! a file it has quoted. See
//! `RECORD/2026-09-19.a-corpus-that-edits.completed.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_core::api::{Counts, SessionView};
use luu::export::read_record;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

/// A copy of `scripts/tasks/edit-reread/`, because the corpus edits it.
///
/// The template is committed and this is the only thing that runs against it,
/// which is the half the 2026-09-15 write-tools probe did not have: its
/// template lived in `/tmp` and was never written down, so that run cannot be
/// repeated.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("luu-edit-reread-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("a scratch tree");
    let template = root().join("scripts/tasks/edit-reread/src");
    for entry in std::fs::read_dir(&template)
        .expect("the template")
        .flatten()
    {
        let to = dir.join("src").join(entry.file_name());
        std::fs::copy(entry.path(), &to).expect("copying the template");
    }
    dir
}

/// The mock's replies, one per **model call** rather than one per turn: a turn
/// that calls a tool makes two, and the list has to line up with that or every
/// edit lands on the wrong turn.
///
/// Written out rather than cycled, for the reason `scripts/make-fixtures.sh`
/// writes its tool-call fixtures out: `--mock-cycle` would put an edit in every
/// turn, and then the control is not a control. The count is asserted below, so
/// a corpus that gains a prompt breaks this loudly instead of quietly editing
/// one turn early.
fn replies() -> Vec<String> {
    let edit = |path: &str, old: &str, new: &str| {
        format!(
            "Let me change it.\n```tool\n{}\n```",
            serde_json::json!({
                "name": "edit_file",
                "arguments": {"path": path, "old_string": old, "new_string": new},
            })
        )
    };
    // The `old_string` each edit targets is the one occurrence in the file:
    // `edit_file` refuses an ambiguous replacement, and the fixtures mention
    // what they are for in their own doc comments.
    vec![
        "greet returns a greeting.".into(), // 1  what does greet return
        "render pads to WIDTH.".into(),     // 2  the control, first read
        "banner prints a line.".into(),     // 3  the blind spot, read once
        "THRESHOLD is 16.".into(),          // 4  the overlap case, 1-6
        edit("src/greeting.rs", "Hello, {name}", "Hi, {name}"), // 5  the first edit
        "Edited.".into(),                   // 5  and the turn's answer
        "greet returns a greeting.".into(), // 6  the first divergence
        "render pads to WIDTH.".into(),     // 7  the control, second read
        edit("src/banner.rs", "\"luu\"", "\"luu 0.2\""), // 8  edited, never re-read
        "Edited.".into(),                   // 8
        edit(
            "src/tally.rs",
            "THRESHOLD: usize = 16",
            "THRESHOLD: usize = 32",
        ), // 9
        "Edited.".into(),                   // 9
        "THRESHOLD is 32.".into(),          // 10 the overlap case, 1-16
        edit("src/greeting.rs", "Hi, {name}", "Hey, {name}"), // 11 the second edit
        "Edited.".into(),                   // 11
        "greet returns a greeting.".into(), // 12 the second divergence
        "greeting, banner and tally have changed.".into(), // 13 attaches nothing
    ]
}

/// One run of the corpus, returning the whole tally and the tree it left
/// behind.
///
/// Through the fold rather than over the lines, which is where this tally moved
/// on 2026-09-19: a recording read back through `SessionView` is what both the
/// live server and `luu export` see, and it carries the other five findings
/// beside the one this probe was written for. See
/// `RECORD/2026-09-19.one-counting-surface.completed.md`.
fn run(name: &str, extra: &[&str]) -> (Counts, PathBuf) {
    let dir = scratch(name);
    let record = dir.join("run.jsonl");
    let corpus = root().join("scripts/tasks/edit-reread.txt");

    let mut command = Command::new(env!("CARGO_BIN_EXE_luu"));
    command
        .current_dir(&dir)
        // A state directory of its own, empty: without it the run reads the
        // `config.toml` of whoever runs the tests, and a `default` written
        // there sends this corpus to their model instead of the mock.
        .env("LUU_HOME", dir.join(".luu-home"))
        .arg("chat")
        .args(["--script", corpus.to_str().expect("a utf-8 path")])
        .args(["--allow-write", "."])
        .args(["--context-limit", "8192"])
        .args(["--mock-delay-ms", "0"])
        .args(extra)
        .arg("--record")
        .arg(&record);
    for reply in replies() {
        command.args(["--mock-reply", &reply]);
    }

    let status = command
        .stdout(std::process::Stdio::null())
        .status()
        .expect("running luu");
    assert!(status.success(), "{name} exited with {status}");

    let lines = read_record(&record).expect("reading the recording back");
    (SessionView::from_record(name, &lines).counts(), dir)
}

/// The corpus produces the case, and the count is the thing worth reading.
///
/// **Under `--no-repeat-once`, which is where this number lives now.** Rule A
/// off is where a prompt carrying one path twice is the documented behaviour:
/// every turn re-sends what it selected, and the blocks read as per-turn
/// snapshots. The default repairs it, which the test below pins, and keeping
/// both arms is what makes the repair a measurement rather than an assertion.
///
/// **Eight renders, not two.** Turn 6 is the first — two bodies of
/// `src/greeting.rs`, the stale one from turn 1 and the fresh one it just read
/// — and turn 12 is the second read after the second edit, three bodies by
/// then. The other six are turns 7 to 11 and 13, which attach nothing of that
/// file and carry the contradiction anyway, because it is *in the history*: a
/// turn's `code_context` is written once and every later prompt renders it. So
/// a divergence is not an event at the turn that causes it. It is a state the
/// window enters and does not leave until eviction takes the stale turn, and
/// two edits in a thirteen-turn session left 8 of 13 prompts self-contradictory.
///
/// `asking` is 2 and not 8, and the gap is the whole of why that field exists:
/// only turns 6 and 12 disagreed with the disk *as it was when they were sent*.
/// The other six disagreed with a version of the file that no longer existed
/// anywhere.
#[test]
fn the_corpus_contradicts_itself_about_one_file_and_no_other() {
    let (found, dir) = run("fragments", &["--no-repeat-once"]);

    assert_eq!(
        found.diverged.paths,
        vec![("src/greeting.rs:1-10".to_string(), 3)],
        "one path, three bodies at its worst — and nothing else reported",
    );
    assert_eq!(
        found.diverged.renders, 8,
        "turns 6 to 13: the contradiction enters the history and stays — {found:?}",
    );
    assert_eq!(
        found.diverged.asking, 2,
        "only turns 6 and 12 re-read the file they contradicted",
    );
    assert_eq!(found.diverged.lines, 8, "one path per render");
    assert_eq!(
        found.superseded.renders, 0,
        "rule A's repair cannot reach an arm rule A is off in: {found:?}",
    );

    // The edits happened. Without this the three silences below would be
    // indistinguishable from a run whose tool calls all failed, which is
    // exactly how the first end-to-end reproduction of this defect nearly went
    // unnoticed: a binary that predated the detector wrote a recording that
    // looked completely normal.
    let read = |name: &str| {
        std::fs::read_to_string(dir.join("src").join(name)).expect("reading the scratch tree")
    };
    assert!(read("greeting.rs").contains("Hey, {name}"), "two edits");
    assert!(read("banner.rs").contains("luu 0.2"), "edited once");
    assert!(read("tally.rs").contains("= 32"), "edited once");
    assert!(
        read("format.rs").contains("WIDTH: usize = 12"),
        "the control is never edited",
    );
}

/// The fix, on the corpus that produced the number it was waiting for.
///
/// The same eight renders as the arm above, and **nothing left to contradict**:
/// `diverged` is 0 because one body goes out per path, and `superseded` is what
/// the render did instead. Ten spans over eight renders and not eight, because
/// after the second edit a single prompt carries two stale bodies — turn 1's
/// and turn 6's — and replaces both.
///
/// The cost, measured against this same corpus on the binary one commit back
/// and recorded rather than asserted here: session prefix reuse 93.96% →
/// 87.11%, **all of it in turns 6 and 12** (90.5% → 47.6% and 93.6% → 62.9%),
/// with every other turn inside 0.3 points of where it was. The break is one
/// per *edit* and not one per render, because the citation that replaces a
/// stale block is itself stable — which is the ratchet, and the reason the
/// feared cost did not arrive. The prompt also got *smaller*: the history
/// bucket over the session fell 7 238 → 6 470 tokens, since a citation is
/// cheaper than the bytes nobody could trust. See
/// `RECORD/runs/2026-09-20.the-newest-body-wins/`.
#[test]
fn the_default_arm_sends_one_body_and_cites_the_rest() {
    let (found, _) = run("superseded", &[]);

    assert_eq!(
        found.diverged.renders, 0,
        "the fix leaves nothing for the detector to report: {found:?}",
    );
    assert_eq!(
        found.superseded.renders, 8,
        "the same eight renders the other arm contradicts itself on: {found:?}",
    );
    assert_eq!(found.superseded.lines, 8, "one path per render");
    assert_eq!(
        found.superseded.spans, 10,
        "two of the eight renders carry two stale bodies, not one",
    );
    assert_eq!(
        found.superseded.paths,
        vec![("src/greeting.rs:1-10".to_string(), 10)],
        "one path, and nothing else replaced",
    );
    assert!(
        found.superseded.tokens.tokens > 0,
        "a repair with no tokens in it is a repair nobody can weigh: {:?}",
        found.superseded.tokens,
    );
    assert_eq!(
        found.superseded.tokens.counters.len(),
        1,
        "one session, one counter — a mixed total would mean a resume",
    );
}

/// Rule A's own number, on the corpus that was built for a different question.
///
/// Nothing could produce this before `record::FORMAT` 14: `split_shown`
/// computed it, the render subtracted it from the `history` bucket, and a
/// difference is not a measurement of what made it.
///
/// Read beside `the_corpus_contradicts_itself_about_one_file_and_no_other`, the
/// two numbers are item 19's mechanism as two figures rather than as a
/// paragraph: **rule A collapsed 7 spans on 7 renders of the same session in
/// which 8 renders carried a path under two bodies.** It is not collapsing the
/// contradiction, and it cannot — a fragment's identity is its path *and* its
/// bytes, so the one case where a span genuinely needs re-reading is precisely
/// the case the dedup cannot see. See
/// `RECORD/2026-09-19.one-path-two-bodies.completed.md` §Why it happens.
///
/// `asking` is 1: exactly one render had its *fresh* read dropped because an
/// older turn in the same prompt was already showing that body — which is the
/// control file, read twice and never edited.
#[test]
fn rule_a_collapses_the_spans_it_can_see_and_not_the_ones_that_moved() {
    let (found, _) = run("repeats", &[]);

    assert_eq!(found.repeated.renders, 7);
    assert_eq!(found.repeated.spans, 7, "one span per render it fired on");
    assert_eq!(
        found.repeated.asking, 1,
        "the control, whose two reads are one body",
    );
    assert!(
        found.repeated.tokens.tokens > 0,
        "a saving with no tokens in it is a saving nobody can weigh: {:?}",
        found.repeated.tokens,
    );
    assert_eq!(
        found.repeated.tokens.counters.len(),
        1,
        "one session, one counter — a mixed total would mean a resume",
    );
    assert!(
        found.superseded.renders > found.repeated.renders,
        "rule A cannot collapse the bodies that moved — it replaces them \
         instead, which is the fix and is why this is the pair to read: \
         {found:?}",
    );
}

/// The three files that must stay quiet, and the three different reasons.
///
/// `format.rs` is read twice and never edited, so the two renders are one body
/// and rule A collapses them — the detector is right to say nothing, and a
/// detector that named this file would be naming every file. The other two are
/// **blind spots** rather than successes: `banner.rs` is edited after its only
/// read, so the window keeps sending bytes that are no longer on disk with
/// nothing to contradict them; `tally.rs` is grounded `1-6` and then `1-16`,
/// two spec strings that overlap on the line that changed and that the
/// detector counts as two paths.
#[test]
fn the_control_stays_quiet_and_so_do_the_two_blind_spots() {
    // Both arms, because a silence that only holds on one of them is a fact
    // about the flag and not about the corpus. The blind spots are blind to
    // the repair for exactly the reason they are blind to the detector: it
    // only ever reads what the window carries twice.
    for (arm, extra) in [
        ("silences", &[][..]),
        ("silences-always", &["--no-repeat-once"]),
    ] {
        let (found, _) = run(arm, extra);
        let named: Vec<&str> = found
            .diverged
            .paths
            .iter()
            .chain(found.superseded.paths.iter())
            .map(|(path, _)| path.as_str())
            .collect();

        for quiet in ["src/format.rs", "src/banner.rs", "src/tally.rs"] {
            assert!(
                !named.iter().any(|path| path.starts_with(quiet)),
                "{arm}: {quiet} was reported: {named:?}",
            );
        }
    }
}

/// The arm one flag apart, and the reason the template is Rust.
///
/// `--select-tokens` lets relevance selection put a span in a turn nobody
/// attached it to, which is the path a session takes on its own — and
/// `agent_core::repo_map` indexes `.rs` and nothing else, so a template in any
/// other language could only ever be grounded by hand. The assertion is the
/// weaker one on purpose: what the selector chooses is scored against the
/// prompt and is not this test's to pin, so what is pinned is that the flag
/// cannot *lose* the divergences the fragments already produce.
///
/// What it produced on 2026-09-19, recorded rather than asserted: sixteen
/// lines over the same eight renders, because the selector re-read
/// `src/greeting.rs:8-10` — `greet` itself, a span **nobody typed** — beside
/// the `1-10` the corpus attaches. That is the case this arm exists for: a
/// divergence a session produces on its own. See
/// `RECORD/runs/2026-09-19.edit-reread-mock/selected.diverged.txt`.
#[test]
fn selection_is_an_arm_and_not_a_different_corpus() {
    let (found, _) = run("selected", &["--select-tokens", "1024", "--no-repeat-once"]);

    assert!(
        found.diverged.renders >= 8,
        "selection may add divergences and may not take one away: {found:?}",
    );
    assert!(
        found.diverged.lines >= found.diverged.renders,
        "at least one path per render: {found:?}",
    );
    assert!(
        found
            .diverged
            .paths
            .iter()
            .any(|(path, bodies)| path.starts_with("src/greeting.rs") && *bodies >= 3),
        "the file the corpus edits twice is still reported: {found:?}",
    );

    // And under the default, where the same spans the selector found are the
    // spans the repair has to reach. Sixteen lines over the same eight
    // renders, because `src/greeting.rs:8-10` — `greet` itself, a span nobody
    // typed — is chosen beside the `1-10` the corpus attaches, and both moved.
    let (found, _) = run("selected-default", &["--select-tokens", "1024"]);
    assert_eq!(
        found.diverged.renders, 0,
        "a span the selector found is repaired like any other: {found:?}",
    );
    assert!(
        found.superseded.lines >= found.superseded.renders,
        "at least one path per render: {found:?}",
    );
    assert!(
        found
            .superseded
            .paths
            .iter()
            .any(|(path, _)| path == "src/greeting.rs:8-10"),
        "the span nobody typed is one of them: {found:?}",
    );
}

/// The template is four leaf files, and the fifth one is the point.
///
/// A fixture inside this repository is inside **this repository's own repo
/// map**: `agent_core::repo_map`'s walk skips dot-directories and `target/`
/// and nothing else, so anything `.rs` under `scripts/` is a candidate for
/// `luu map` and `luu select` here. The four leaves cost nothing measurable —
/// every verdict across `select_probe`'s 38 questions is identical with them
/// and without them, at both budgets. A `src/lib.rs` carrying four `pub mod`
/// lines cost the graph-hop arm **two first-place hits and four top-3 hits**,
/// because those lines are edges and the graph signal follows them.
///
/// So: no `Cargo.toml`, no `lib.rs`, no `mod.rs`. Nothing builds this tree and
/// nothing should link it together either. The measurement is in
/// `RECORD/2026-09-19.a-corpus-that-edits.completed.md` §What it cost, and the
/// reason it is a test rather than a comment is that `select_probe` asserts the
/// selector beats path order and not the numbers it printed last week — so a
/// fixture that quietly moved them would not be caught by the probe it moved.
#[test]
fn the_corpus_tree_is_a_fixture_and_not_a_crate() {
    let tree = root().join("scripts/tasks/edit-reread");
    for forbidden in ["Cargo.toml", "src/lib.rs", "src/mod.rs"] {
        assert!(
            !tree.join(forbidden).exists(),
            "{forbidden} is in the corpus's tree, which puts it in this \
             repository's own map: see this test's doc comment for what the \
             last one cost",
        );
    }
}
