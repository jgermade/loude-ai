//! The tool-call probe's harness: proved without a model.
//!
//! `select_probe.rs` computes coverage on every commit because it is a
//! structural question a fixture can answer. This is the same shape for the
//! four-way question roadmap item 4 asked for — *parsed* / *drifted* /
//! *no call* / *continued past the fence* — driven end to end through
//! `--mock-cycle`'s own mechanism (`Mock::cycle`) rather than by calling
//! `score_call` on a bare string, so what this test proves is that the
//! instrument, the corpus and the scorer compose, not only that the scorer
//! works in isolation (that is `tools::tests::score_call_*`, in
//! `agent-core`). See `RECORD/2026-09-14.the-tool-call-probe.completed.md`.
//!
//! This is *not* a claim about a model. Nothing here streams a real reply;
//! it proves the harness recovers four fixed outcomes when fed them, so that
//! running it against a model later is the only new variable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_core::backend::mock::Mock;
use agent_core::backend::{Backend, Chunk, CompletionRequest, Message};
use agent_core::tools::{CallVerdict, Tools, score_call};
use futures_util::StreamExt;

fn corpus() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/tasks/tool-call-probe.txt")
        .canonicalize()
        .expect("the corpus is on disk");
    read_prompts(&path)
}

fn read_prompts(path: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

async fn text_of(backend: &dyn Backend, prompt: &str) -> String {
    let request = CompletionRequest {
        tools: Vec::new(),
        model: "mock".into(),
        messages: vec![Message::user(prompt)],
        context_limit: None,
        temperature: None,
        seed: None,
        constraint: None,
    };
    let mut stream = backend.stream(request);
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        if let Chunk::Text(word) = chunk.expect("the mock never fails without .failing()") {
            text.push_str(&word);
        }
    }
    text
}

#[tokio::test]
async fn the_probe_recovers_all_four_verdicts_through_a_cycling_mock() {
    let prompts = corpus();
    assert_eq!(prompts.len(), 15, "the corpus is fifteen prompts by design");

    // One fixture reply per verdict, in this fixed order, cycling through the
    // corpus — exactly the shape --mock-cycle exists to make possible.
    let replies = vec![
        // Parsed: a clean call, nothing after it.
        "looking\n```tool\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"AGENTS.md\"}}\n```"
            .to_string(),
        // Continued past the fence: the call parses, then the model keeps
        // writing — the precision run's failure, reproduced as a fixture.
        "looking\n```tool\n{\"name\": \"list_dir\", \"arguments\": {\"path\": \".\"}}\n```\nThe directory holds the usual crates, plus scripts and RECORD."
            .to_string(),
        // Drifted: fenced under the tool's own name instead of ```tool.
        "```read_file\n{\"path\": \"AGENTS.md\"}\n```".to_string(),
        // No call: plain prose, no attempt.
        "AGENTS.md opens with a short project description.".to_string(),
    ];
    let backend = Mock::replies(replies).cycle(true).delay(Duration::ZERO);

    let tool_names: Vec<&str> = Tools::standard().names().collect();
    let mut counts = [0usize; 4];
    for prompt in &prompts {
        let text = text_of(&backend, prompt).await;
        let verdict = score_call(&text, &tool_names);
        counts[match verdict {
            CallVerdict::Parsed => 0,
            CallVerdict::ContinuedPastFence => 1,
            CallVerdict::Drifted => 2,
            CallVerdict::NoCall => 3,
        }] += 1;
    }

    // Fifteen prompts over a four-reply cycle: four full rounds and one
    // extra, so the first verdict (Parsed) lands one more time than the rest.
    assert_eq!(counts, [4, 4, 4, 3]);
}
