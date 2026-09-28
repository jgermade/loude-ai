//! The one assertion the store has to keep passing.
//!
//! ```text
//! fold(record) -> SessionView  ==  save(store, it) ; load(store, id) -> SessionView
//! ```
//!
//! The store is a **cache of a fold, not a second log** — that is the whole of
//! `luu-design.md` §Persistence, and this is what stops it becoming one. A
//! store that can drift is a store that will, and without this the drift is a
//! support question years later instead of a red test now.
//!
//! The recordings are **produced by running the binary**, the same way
//! `scripts/make-fixtures.sh` produces the ones the static deploy replays —
//! not hand-written `RecordLine` vectors. A fixture written by hand can only
//! contain the variants whoever wrote it remembered; one produced by a run
//! contains what the code actually emits, including whatever lands next
//! without anyone remembering to update this file.
//!
//! Argued in `RECORD/2026-09-02.sessions-in-sqlite.completed.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_core::api::SessionView;
use luu::export::read_record;
use luu::store::SessionStore;

/// The repository root, which is where the scripts and `luu.toml` are relative
/// to. Cargo runs an integration test from its package directory.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

/// A directory per recording, not per process: the tests in this file run on
/// their own threads and record the same names, and a shared path makes one
/// test read the half-written file of another.
fn scratch() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("luu-store-parity-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// One recorded run of the real binary. `--mock-delay-ms 0` throughout: the
/// fixtures the deploy replays are paced so a human can watch them, and this
/// wants the same *messages* without the twenty-seven seconds of watching.
fn record(name: &str, args: &[&str]) -> Vec<agent_core::record::RecordLine> {
    let path = scratch().join(format!("{name}.jsonl"));
    let status = Command::new(env!("CARGO_BIN_EXE_luu"))
        .current_dir(root())
        // A state directory of its own, for `edit_reread_probe`'s reason: the
        // `config.toml` of whoever runs the tests is not this test's input.
        .env("LUU_HOME", scratch().join("home"))
        .arg("chat")
        .args(args)
        .args(["--mock-delay-ms", "0"])
        .arg("--record")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("running luu");
    assert!(
        status.success() || name == "backend-failure",
        "{name} exited with {status}",
    );
    read_record(&path).expect("reading the recording back")
}

/// Recordings covering every kind of line the format has: turns, the task
/// lifecycle with a close, eviction, tool calls with both verdicts, a fused
/// fragment, and a turn that failed.
fn recordings() -> Vec<(String, Vec<agent_core::record::RecordLine>)> {
    let policy = root().join("luu.toml");
    let policy = policy.to_str().expect("a utf-8 path");
    let tasks = root().join("scripts/tasks/one-task.txt");
    let tasks = tasks.to_str().expect("a utf-8 path");

    vec![
        (
            "completed-turn".into(),
            record("completed-turn", &["hola", "--context-limit", "8192"]),
        ),
        (
            // A window small enough that the history has to give way, so the
            // recording carries `evicted` lines — what a session forgot, which
            // nothing can work out afterwards.
            "eviction".into(),
            record(
                "eviction",
                &[
                    "--script",
                    root()
                        .join("scripts/tasks/steady-state.txt")
                        .to_str()
                        .expect("a utf-8 path"),
                    "--context-limit",
                    "1024",
                    "--reserve",
                    "64",
                ],
            ),
        ),
        (
            // The one shape where the history is rewritten rather than dropped:
            // a task proposed, approved, run and closed to a summary.
            "one-task".into(),
            record(
                "one-task",
                &[
                    "--script",
                    tasks,
                    "--context-limit",
                    "8192",
                    "--sandbox",
                    policy,
                ],
            ),
        ),
        (
            // The tool loop with the sandbox answering both ways, so the fold
            // carries a `ToolCallView` with each verdict.
            "tool-calls".into(),
            record(
                "tool-calls",
                &[
                    "What is in AGENTS.md, and what is in /etc/hostname?",
                    "--context-limit",
                    "8192",
                    "--sandbox",
                    policy,
                    "--mock-reply",
                    "Let me read it.\n```tool\n{\"name\":\"read_file\",\
                     \"arguments\":{\"path\":\"AGENTS.md\",\"max_lines\":3}}\n```",
                    "--mock-reply",
                    "Now the other.\n```tool\n{\"name\":\"read_file\",\
                     \"arguments\":{\"path\":\"/etc/hostname\"}}\n```",
                    "--mock-reply",
                    "The second path is outside the sandbox.",
                ],
            ),
        ),
        (
            // A real file fused into a turn: the `code` bucket, which the
            // fusion rule keeps apart from the prompt.
            "grounded-turn".into(),
            record(
                "grounded-turn",
                &[
                    "which two commitments does this file open with?",
                    "--fragment",
                    "crates/agent-core/src/context.rs:1-15",
                    "--context-limit",
                    "8192",
                    "--sandbox",
                    policy,
                ],
            ),
        ),
        (
            // The failure path, without depending on a backend being there.
            "backend-failure".into(),
            record(
                "backend-failure",
                &[
                    "anything",
                    "--backend",
                    "ollama",
                    "--ollama-url",
                    "http://127.0.0.1:1",
                ],
            ),
        ),
    ]
}

#[test]
fn what_the_store_gives_back_is_what_folding_the_record_produces() {
    let store = SessionStore::in_memory().expect("a store");
    let recordings = recordings();
    assert!(
        recordings.iter().all(|(_, lines)| lines.len() > 1),
        "a recording with only a header proves nothing",
    );

    for (id, lines) in &recordings {
        let folded = SessionView::from_record(id.clone(), lines);
        store.save(&folded, None).expect("saving the fold");
        let loaded = store
            .load(id)
            .expect("loading it back")
            .unwrap_or_else(|| panic!("{id} was saved and is not there"));

        assert_eq!(
            serde_json::to_value(&loaded).expect("the stored fold"),
            serde_json::to_value(&folded).expect("the folded record"),
            "the store disagrees with the record for {id}",
        );
    }

    // And the listing is the same set, derived from the same folds rather than
    // accumulated beside them.
    let listed = store.list().expect("listing");
    assert_eq!(listed.len(), recordings.len());
    for (id, lines) in &recordings {
        let summary = SessionView::from_record(id.clone(), lines).summary();
        let row = listed
            .iter()
            .find(|row| &row.id == id)
            .unwrap_or_else(|| panic!("{id} is not in the listing"));
        assert_eq!(
            serde_json::to_value(row).expect("the listed row"),
            serde_json::to_value(&summary).expect("the fold's own summary"),
        );
    }
}

/// The half of the rule the round trip alone does not cover: what comes back
/// has to survive being read by the *fold's* own reader, not just by
/// `serde_json`. A stored view that only round-trips through this test's
/// comparison could still be missing a field every client reads.
#[test]
fn a_stored_fold_still_answers_the_read_side_questions() {
    let store = SessionStore::in_memory().expect("a store");
    let (id, lines) = recordings()
        .into_iter()
        .find(|(id, _)| id == "one-task")
        .expect("the task recording");
    let folded = SessionView::from_record(id.clone(), &lines);
    store.save(&folded, None).expect("saving");

    let loaded = store.load(&id).expect("loading").expect("the session");
    assert!(!loaded.turns.is_empty(), "a session with no turns");
    assert!(!loaded.jobs.is_empty(), "the job lifecycle is in the fold");
    let closed = loaded
        .jobs
        .iter()
        .find(|job| job.summary.is_some())
        .expect("a closed job");
    assert!(
        !closed.summary.as_deref().unwrap_or_default().is_empty(),
        "the summary is what its turns are sent as; an empty one is a fold that lost the work",
    );
    assert_eq!(
        loaded.turn(1).map(|turn| turn.turn),
        Some(1),
        "the view's own accessors work on what came out of the store",
    );
}

#[test]
fn a_resumed_context_produces_matching_budget_and_prompt_selection() {
    use agent_core::context::{ApproximateCounter, Budget, Eviction};

    let store = SessionStore::in_memory().expect("a store");
    let (id, lines) = recordings()
        .into_iter()
        .find(|(id, _)| id == "one-task")
        .expect("the task recording");
    let folded = SessionView::from_record(id.clone(), &lines);
    store.save(&folded, None).expect("saving");

    let counter = ApproximateCounter;
    // No sandbox: this asks whether the *fold* comes back, and `one-task`
    // grounds nothing. What a sandbox restores is `edit_reread_probe.rs`'s and
    // `context.rs`'s question.
    let restored = store
        .resume(
            &id,
            "system preamble",
            "tool definitions",
            "",
            &counter,
            None,
        )
        .expect("resuming")
        .expect("session exists");
    assert!(restored.unreadable.is_empty(), "nothing to read back");
    let mut resumed = restored.context;

    assert_eq!(resumed.turns().len(), folded.turns.len());
    assert_eq!(resumed.jobs().len(), folded.jobs.len());

    let summary_text = resumed
        .tasks()
        .iter()
        .find(|t| t.is_closed())
        .and_then(|t| t.summary.as_ref())
        .map(|s| s.text.clone())
        .expect("closed task in resumed context");

    let budget = Budget::new(8192, 512, Eviction::Turn);
    let selection = resumed.select("next question", &[], budget, &counter);

    // Summary message is in the prompt
    assert!(
        selection.messages.iter().any(|m| m.content == summary_text),
        "folded summary message is in the prompt"
    );
    // System message is in the prompt
    assert_eq!(
        selection.messages[0].content,
        "system preamble\n\ntool definitions"
    );
    // Reserve bucket is present
    assert!(
        selection
            .buckets
            .iter()
            .any(|b| b.name == "summaries" && b.tokens > 0)
    );
    assert!(
        selection
            .buckets
            .iter()
            .any(|b| b.name == "prompt" && b.tokens > 0)
    );

    // And eviction floor survives resume
    let (ev_id, ev_lines) = recordings()
        .into_iter()
        .find(|(id, _)| id == "eviction")
        .expect("the eviction recording");
    let ev_folded = SessionView::from_record(ev_id.clone(), &ev_lines);
    store.save(&ev_folded, None).expect("saving eviction");

    let resumed_ev = store
        .resume(&ev_id, "sys", "", "", &counter, None)
        .expect("resuming eviction")
        .expect("eviction session exists");

    assert!(
        resumed_ev.context.floor() > 0,
        "eviction floor survived resume: {}",
        resumed_ev.context.floor()
    );
}

/// A grounded turn says so on the wire, and a resume reads its span again.
///
/// End to end, through the real binary: `grounded-turn` is recorded with
/// `--fragment crates/agent-core/src/context.rs:1-15`, so the stream carries a
/// `grounded` line naming that spec and **not** its bytes. Folding, saving,
/// loading and resuming brings the span back by reading the file as it is
/// now — which is the whole of item 20, and the reason the assertion compares
/// against the disk rather than against a string in this file.
///
/// The pair with `a_resumed_context_produces_matching_budget_and_prompt_selection`
/// one test up: that one resumes with no sandbox and asks whether the *fold*
/// comes back, this one resumes with one and asks whether the *code* does. See
/// `RECORD/2026-09-19.fragments-by-reference.completed.md`.
#[test]
fn a_resumed_turn_gets_its_span_back_by_reading_it_again() {
    use agent_core::context::ApproximateCounter;
    use agent_core::sandbox::{Access, PathRule, Sandbox, SandboxPolicy};

    let spec = "crates/agent-core/src/context.rs:1-15";
    let (id, lines) = recordings()
        .into_iter()
        .find(|(id, _)| id == "grounded-turn")
        .expect("the grounded recording");

    // The emitter, before the fold: a stream that does not say a turn was
    // grounded is a stream no resume can restore from, and it would fail this
    // test one indirection further along where the cause is harder to read.
    assert!(
        lines.iter().any(|line| matches!(
            line,
            agent_core::record::RecordLine::Protocol {
                message: agent_core::protocol::ServerMessage::Grounded { spans, .. },
                ..
            } if spans.iter().any(|one| one.spec == spec)
        )),
        "the recording carries no `grounded` line naming {spec}",
    );

    let store = SessionStore::in_memory().expect("a store");
    let folded = SessionView::from_record(id.clone(), &lines);
    store.save(&folded, None).expect("saving");
    assert_eq!(
        folded.turns[0].code.len(),
        1,
        "the fold carries the reference",
    );

    let sandbox = Sandbox::new(
        &SandboxPolicy {
            paths: vec![PathRule::new(".", Access::Read)],
            ..SandboxPolicy::default()
        },
        &root(),
    )
    .expect("a sandbox over the repository");

    let restored = store
        .resume(
            &id,
            "system",
            "tools",
            "",
            &ApproximateCounter,
            Some(&sandbox),
        )
        .expect("resuming")
        .expect("session exists");
    assert!(restored.unreadable.is_empty(), "{:?}", restored.unreadable);

    let code = &restored.context.turns()[0].code_context;
    assert_eq!(code.len(), 1, "one span, restored");
    assert_eq!(code[0].path, spec, "the spec, as the turn named it");

    let on_disk: String = std::fs::read_to_string(root().join("crates/agent-core/src/context.rs"))
        .expect("the file the fragment names")
        .lines()
        .take(15)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        code[0].text.trim_end(),
        on_disk.trim_end(),
        "restored by reading the file again, not from anything the store held",
    );
}

/// The person's spans and the selector's are told apart, once, where it is
/// still possible.
///
/// `chat` joins them into one `Vec<Fragment>` — the person's first, keeping
/// their whole budget — and from that line on nothing downstream can take them
/// apart. So the boundary is caught at the join and carried on the wire, and
/// this is what says it still is: one run with both a typed `--fragment` and
/// `--select-tokens` on, and the `grounded` line naming which is which.
///
/// It matters because the two have different claims on being restored across a
/// resume — see `RECORD/2026-09-19.fragments-by-reference.completed.md`
/// §Two things — and because in `serve` there is no `--fragment` at all, so
/// every span in a session the store holds is `Selected`. A reader who cannot
/// tell would have to guess which of those two worlds a recording came from.
#[test]
fn a_typed_fragment_and_a_chosen_span_are_not_the_same_kind_of_thing() {
    use agent_core::protocol::{Origin, ServerMessage};
    use agent_core::record::RecordLine;

    let policy = root().join("luu.toml");
    let lines = record(
        "grounded-origins",
        &[
            "which two commitments does this file open with?",
            "--fragment",
            "crates/agent-core/src/context.rs:1-15",
            "--select-tokens",
            "1024",
            "--context-limit",
            "8192",
            "--sandbox",
            policy.to_str().expect("a utf-8 path"),
        ],
    );

    let spans = lines
        .iter()
        .find_map(|line| match line {
            RecordLine::Protocol {
                message: ServerMessage::Grounded { spans, .. },
                ..
            } => Some(spans.clone()),
            _ => None,
        })
        .expect("a grounded line");

    assert_eq!(
        spans[0].origin,
        Origin::Attached,
        "the person's goes first and is named as theirs: {spans:?}",
    );
    assert_eq!(spans[0].spec, "crates/agent-core/src/context.rs:1-15");
    assert!(
        spans[1..].iter().all(|one| one.origin == Origin::Selected),
        "everything after the boundary is the selector's: {spans:?}",
    );
    assert!(
        spans.len() > 1,
        "the selector found nothing, so this run proves only half of what it is for",
    );
}
