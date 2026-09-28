//! Integration tests for `luu stdio` protocol stream.
//!
//! Asserts that the NDJSON line-oriented protocol over stdin/stdout behaves
//! identically to the WebSocket protocol: Hello greeting, task proposal,
//! approval, turn execution, refusal on busy, and graceful EOF shutdown.

use std::sync::Arc;
use std::time::Duration;

use agent_core::backend::mock::Mock;
use agent_core::context::{ApproximateCounter, Budget, Eviction};
use agent_core::protocol::{ClientMessage, ServerMessage};
use agent_core::repo_map::Order;
use agent_core::sandbox::{Sandbox, SandboxPolicy};
use agent_core::tools::Tools;
use luu::serve::{StdioOptions, serve_stdio_stream};
use luu::session::Agency;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PLAN: &str = "```plan\n{\"objective\":\"explain stdio\",\"steps\":[\"describe it\"],\
                    \"files\":[],\"commands\":[]}\n```";

const ANSWER: &str = "The stdio protocol is line-oriented NDJSON.";

fn options_for(replies: Vec<String>) -> StdioOptions {
    let backend = Arc::new(Mock::replies(replies).delay(Duration::ZERO));
    let base = std::env::current_dir().expect("current dir");
    let sandbox = Arc::new(Sandbox::new(&SandboxPolicy::default(), &base).expect("open sandbox"));
    let agency = Agency::new(
        Arc::new(Tools::standard()),
        sandbox,
        agent_core::agent::Limits::default().with_max_steps(4),
        None,
    );
    let counter = Arc::new(ApproximateCounter);
    let budget = Budget::new(0, 512, Eviction::Turn);
    StdioOptions {
        // No icon theme in a test: the page is not what is under test.
        icons: std::sync::Arc::new(luu::icons::Theme::default()),
        // Over stdio the process is the session: nothing here chooses a
        // posture, so there is nothing to build one from and nothing to offer.
        agency_for: None,
        postures: Default::default(),
        postures_path: None,
        // `named`: a destination handed to the server, which it keeps. One
        // nobody chose would follow the `config.toml` of whoever runs the
        // tests — and start the engine it names.
        provider: luu::provider::Resolved {
            named: true,
            ..luu::provider::Resolved::mock()
        },
        counter_warning: None,
        approvers: Default::default(),
        backend,
        model: "mock".to_string(),
        record: None,
        budget,
        counter,
        tokenizer: None,
        agency,
        temperature: None,
        seed: None,
        map_tokens: 0,
        map_order: Order::Path,
        map_fill: agent_core::repo_map::Fill::Greedy,
        select_tokens: 0,
        select_weights: Default::default(),
        constrain: None,
        store: None,
    }
}

#[tokio::test]
async fn stdio_greets_with_hello_and_answers_prompts() {
    let options = options_for(vec![PLAN.to_string(), ANSWER.to_string()]);

    // Set up duplex streams for client <-> server communication.
    // client writes to client_out (server reads from server_in).
    // server writes to server_out (client reads from client_in).
    let (server_in, mut client_out) = tokio::io::duplex(4096);
    let (client_in, server_out) = tokio::io::duplex(4096);

    let server_handle = tokio::spawn(async move {
        let reader = BufReader::new(server_in);
        serve_stdio_stream(options, reader, server_out).await
    });

    let mut client_lines = BufReader::new(client_in).lines();

    // 1. Read Hello message.
    let hello_line = client_lines
        .next_line()
        .await
        .expect("read hello")
        .expect("hello line exists");
    let hello: ServerMessage = serde_json::from_str(&hello_line).expect("parse hello");
    match hello {
        ServerMessage::Hello {
            protocol, backend, ..
        } => {
            assert_eq!(protocol, agent_core::protocol::VERSION);
            assert_eq!(backend, "mock");
        }
        other => panic!("expected Hello, got {other:?}"),
    }

    // 2. Send prompt.
    let prompt_msg = serde_json::to_string(&ClientMessage::Prompt {
        text: "how does stdio work?".to_string(),
    })
    .unwrap();
    client_out
        .write_all(format!("{prompt_msg}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    // Expect TurnStarted, Token(s), Ended, DraftOpened and PlanProposed — the
    // mock's answer is a plan block, so the model suggests one.
    let mut proposed = false;
    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        if matches!(msg, ServerMessage::PlanProposed { .. }) {
            proposed = true;
            break;
        }
    }
    assert!(proposed, "received a plan proposal");

    // 3. Approve job.
    let approve_msg = serde_json::to_string(&ClientMessage::ApprovePlan {
        enforcement: None,
        signature: None,
        job: None,
        files: vec![],
        writes: vec![],
        commands: vec![],
        closes_on: None,
        network: None,
        egress: None,
    })
    .unwrap();
    client_out
        .write_all(format!("{approve_msg}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    // Expect JobClosed for the draft, then JobApproved. No turn follows it:
    // the held prompt is gone, and the work starts on the next prompt.
    let mut approved = false;
    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        if let ServerMessage::JobApproved { from, .. } = msg {
            assert_eq!(
                from,
                Some(1),
                "the approval closed the draft it was offered inside"
            );
            approved = true;
            break;
        }
    }
    assert!(approved, "the plan was approved");

    // 4. Dropping client_out sends EOF on server stdin, which causes graceful shutdown.
    drop(client_out);
    let result = server_handle.await.expect("server join");
    assert!(result.is_ok(), "server terminated cleanly on EOF");
}

#[tokio::test]
async fn unparseable_lines_do_not_break_stdio_stream() {
    let options = options_for(vec![PLAN.to_string(), ANSWER.to_string()]);
    let (server_in, mut client_out) = tokio::io::duplex(4096);
    let (client_in, server_out) = tokio::io::duplex(4096);

    let server_handle = tokio::spawn(async move {
        let reader = BufReader::new(server_in);
        serve_stdio_stream(options, reader, server_out).await
    });

    let mut client_lines = BufReader::new(client_in).lines();

    // Read Hello.
    let _ = client_lines.next_line().await.unwrap().unwrap();

    // Send garbage lines followed by a valid prompt.
    client_out.write_all(b"not valid json\n\n\n").await.unwrap();
    let prompt_msg = serde_json::to_string(&ClientMessage::Prompt {
        text: "hello after garbage".to_string(),
    })
    .unwrap();
    client_out
        .write_all(format!("{prompt_msg}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    // Should still receive PlanProposed!
    let mut proposed = false;
    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        if matches!(msg, ServerMessage::PlanProposed { .. }) {
            proposed = true;
            break;
        }
    }
    assert!(proposed, "plan proposed despite preceding invalid lines");

    drop(client_out);
    let _ = server_handle.await.unwrap();
}

#[tokio::test]
async fn a_prompt_while_a_plan_is_on_the_table_is_more_changes() {
    let options = options_for(vec![PLAN.to_string(), ANSWER.to_string()]);
    let (server_in, mut client_out) = tokio::io::duplex(4096);
    let (client_in, server_out) = tokio::io::duplex(4096);

    let server_handle = tokio::spawn(async move {
        let reader = BufReader::new(server_in);
        serve_stdio_stream(options, reader, server_out).await
    });

    let mut client_lines = BufReader::new(client_in).lines();

    // Read Hello.
    let _ = client_lines.next_line().await.unwrap().unwrap();

    // The first prompt runs and the model's answer carries a plan block, so a
    // plan reaches the table.
    let prompt1 = serde_json::to_string(&ClientMessage::Prompt {
        text: "first prompt".to_string(),
    })
    .unwrap();
    client_out
        .write_all(format!("{prompt1}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        if matches!(msg, ServerMessage::PlanProposed { .. }) {
            break;
        }
    }

    // A second prompt while the plan is up is **not** refused any more. It is
    // the answer *more changes*, which is the same answer as decline: the plan
    // comes off the table, nothing closes, and the prompt runs in the draft it
    // is still in. Refusing it would be the old `pending` guard outliving the
    // held prompt it existed to protect.
    let prompt2 = serde_json::to_string(&ClientMessage::Prompt {
        text: "make it two flags".to_string(),
    })
    .unwrap();
    client_out
        .write_all(format!("{prompt2}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    let mut declined = false;
    let mut ran = false;
    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        match msg {
            ServerMessage::Refused { reason, .. } => {
                panic!("a prompt is an answer, not a thing to refuse: {reason:?}")
            }
            ServerMessage::PlanDeclined => declined = true,
            ServerMessage::TurnStarted { prompt, .. } if prompt == "make it two flags" => {
                ran = true;
                break;
            }
            _ => {}
        }
    }
    assert!(declined, "the plan on the table was answered");
    assert!(ran, "and the prompt that answered it ran");

    drop(client_out);
    let _ = server_handle.await.unwrap();
}

#[tokio::test]
async fn close_and_reopen_task_over_stdio() {
    let options = options_for(vec![PLAN.to_string(), ANSWER.to_string()]);
    let (server_in, mut client_out) = tokio::io::duplex(4096);
    let (client_in, server_out) = tokio::io::duplex(4096);

    let server_handle = tokio::spawn(async move {
        let reader = BufReader::new(server_in);
        serve_stdio_stream(options, reader, server_out).await
    });

    let mut client_lines = BufReader::new(client_in).lines();

    // Read Hello.
    let _ = client_lines.next_line().await.unwrap().unwrap();

    // 1. Send prompt.
    let prompt_msg = serde_json::to_string(&ClientMessage::Prompt {
        text: "task test".to_string(),
    })
    .unwrap();
    client_out
        .write_all(format!("{prompt_msg}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        if matches!(msg, ServerMessage::PlanProposed { .. }) {
            break;
        }
    }

    // 2. Approve job.
    let approve_msg = serde_json::to_string(&ClientMessage::ApprovePlan {
        enforcement: None,
        signature: None,
        job: None,
        files: vec![],
        writes: vec![],
        commands: vec![],
        closes_on: None,
        network: None,
        egress: None,
    })
    .unwrap();
    client_out
        .write_all(format!("{approve_msg}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    // Wait for the approval, which is what hands out the job's id.
    let mut job = 0;
    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        if let ServerMessage::JobApproved { job: opened, .. } = msg {
            job = opened;
            break;
        }
    }

    // 3. Close job.
    let close_msg = serde_json::to_string(&ClientMessage::CloseJob { job }).unwrap();
    client_out
        .write_all(format!("{close_msg}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    let mut closed = false;
    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        if let ServerMessage::JobClosed {
            job: closed_job, ..
        } = msg
        {
            assert_eq!(closed_job, job);
            closed = true;
            break;
        }
    }
    assert!(closed, "job was closed");

    // 4. Reopen job.
    let reopen_msg = serde_json::to_string(&ClientMessage::ReopenJob { job }).unwrap();
    client_out
        .write_all(format!("{reopen_msg}\n").as_bytes())
        .await
        .unwrap();
    client_out.flush().await.unwrap();

    let mut reopened = false;
    while let Some(line) = client_lines.next_line().await.unwrap() {
        let msg: ServerMessage = serde_json::from_str(&line).expect("parse server message");
        if let ServerMessage::JobReopened { job: reopened_job } = msg {
            assert_eq!(reopened_job, job);
            reopened = true;
            break;
        }
    }
    assert!(reopened, "job was reopened");

    drop(client_out);
    let _ = server_handle.await.unwrap();
}
