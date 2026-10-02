//! One turn over `/ws`, against the running server.
//!
//! The unit tests in `serve.rs` call the handlers directly, which tests the
//! gate and not the server: nothing there binds a port, upgrades a socket,
//! serializes a `ServerMessage` onto the wire, or asks the read side what it
//! thinks happened. Every bug this repository has had was found by running it,
//! and this is the cheapest place to keep running it.
//!
//! It also asserts the one property the read side has that nothing else checks.
//! `GET /api/...` is folded from the same events the socket carries, so it
//! *cannot* disagree with what a client watched happen — a claim no test had
//! ever made it prove.

use std::sync::Arc;
use std::time::Duration;

use agent_core::approval::{Approval, ApproverKey, Approvers, Signer};
use agent_core::backend::mock::Mock;
use agent_core::context::{ApproximateCounter, Budget, Eviction, TokenCounter};
use agent_core::sandbox::{Access, Enforcement, Sandbox, SandboxPolicy};
use agent_core::tools::Tools;
use futures_util::{SinkExt, StreamExt};
use luu::serve::{ServeOptions, bind};
use luu::session::{Agency, SYSTEM};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const PLAN: &str = "```plan\n{\"objective\":\"add a flag\",\"steps\":[\"read the CLI\"],\
                    \"files\":[\"Cargo.toml\"],\"commands\":[]}\n```";

const ANSWER: &str = "The flag is added in lib.rs.";

/// A socket that fails rather than hangs: a turn that never ends is a bug, and
/// a test that waits for it forever reports nothing about which one.
const PATIENCE: Duration = Duration::from_secs(10);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A plan that names one file, and a turn that tries to read another. The
/// mock answers the planning call, then asks for a tool, then concludes.
const PLAN_FOR_CARGO_TOML: &str = "```plan\n{\"objective\":\"read the manifest\",\
                                   \"steps\":[\"read it\"],\"files\":[\"Cargo.toml\"],\
                                   \"commands\":[]}\n```";

/// A turn that edits a file the plan only said it would read.
const WRITES_SCRATCH: &str = "Writing it.\n```tool\n                              {\"name\":\"write_file\",\"arguments\":\
                              {\"path\":\"Cargo.toml\",\"content\":\"broken\"}}\n```";

const READS_SERVE_RS: &str = "Let me look.\n```tool\n                              {\"name\":\"read_file\",\"arguments\":\
                              {\"path\":\"src/serve.rs\",\"max_lines\":1}}\n```";

/// The server on an ephemeral port, with the mock answering the planning call
/// and then the turn.
async fn server() -> String {
    server_with(vec![PLAN.into(), ANSWER.into()]).await
}

/// The same, at the pace the mock streams by default — a token every 30ms, so
/// a turn is still running when the next message arrives.
async fn server_slow(replies: Vec<String>) -> String {
    server_at(replies, Duration::from_millis(30)).await
}

async fn server_with(replies: Vec<String>) -> String {
    server_at(replies, Duration::ZERO).await
}

/// A server whose *policy* also grants write on `also`, so the gate has room
/// to widen a plan into it: the person at the gate widens up to the policy file
/// and not past it, which is the rule this exists to exercise rather than
/// bypass.
async fn server_writable(replies: Vec<String>, also: &std::path::Path) -> String {
    let mut policy = SandboxPolicy::default();
    policy.allow(
        also.parent().expect("a parent directory"),
        Access::ReadWrite,
    );
    server_with_policy(replies, Duration::ZERO, policy).await
}

/// A server whose policy allows one program, so a plan can declare it and the
/// gate can name it as what closes the task.
async fn server_running(replies: Vec<String>, command: &str) -> String {
    let mut policy = SandboxPolicy::default();
    policy.allow_command(command);
    // `best-effort`, so the test runs where the kernel rung does not: under
    // the default `kernel` a child is denied wherever Landlock is missing,
    // which is macOS and any container whose kernel lacks it. What is being
    // asserted here is the closing rung, and a test that only runs on one
    // kernel asserts it nowhere else.
    policy.enforcement = Enforcement::BestEffort;
    server_with_policy(replies, Duration::ZERO, policy).await
}

async fn server_at(replies: Vec<String>, delay: Duration) -> String {
    server_with_policy(replies, delay, SandboxPolicy::default()).await
}

/// A server with a window small enough that the history has to give way. The
/// limit is measured from the prefix this very process assembles rather than
/// picked by hand: a hard-coded one is how the degenerate 512-token fixtures
/// happened.
async fn server_evicting(replies: Vec<String>) -> String {
    let prefix = ApproximateCounter.count(SYSTEM)
        + ApproximateCounter.count(&Tools::standard().definitions());
    server_full(
        replies,
        Duration::ZERO,
        SandboxPolicy::default(),
        Budget::new(prefix + ROOM_FOR_HISTORY, 0, Eviction::Turn),
    )
    .await
}

/// Room for a couple of the mock's answers and no more, in tokens.
const ROOM_FOR_HISTORY: u32 = 220;

async fn server_with_policy(
    replies: Vec<String>,
    delay: Duration,
    policy: SandboxPolicy,
) -> String {
    server_full(replies, delay, policy, Budget::new(0, 0, Eviction::Turn)).await
}

/// A server that requires a bearer token, with the token in a file only its
/// owner can read — which is what `resolve` insists on.
async fn server_guarded(token: &str) -> (String, std::path::PathBuf) {
    // One directory per call and not per process: two guarded servers in one
    // test binary run in parallel, and with a shared path one could read the
    // file while the other had just truncated it to write — "the auth token
    // file … is empty", one run in three.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "luu-serve-auth-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("token");
    std::fs::write(&path, token).expect("writing the token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("tightening the mode");
    }
    let address = server_authed(
        vec![PLAN.into(), ANSWER.into()],
        Duration::ZERO,
        SandboxPolicy::default(),
        Budget::new(0, 0, Eviction::Turn),
        Some(path.clone()),
    )
    .await;
    (address, dir)
}

async fn server_full(
    replies: Vec<String>,
    delay: Duration,
    policy: SandboxPolicy,
    budget: Budget,
) -> String {
    server_authed(replies, delay, policy, budget, None).await
}

/// How a session's posture is built here: the policy file it names, resolved
/// against this crate's directory, with no worker.
///
/// The real one is the command line's flags with one field replaced. This is
/// the same shape and the same rules — an explicit file that is not there is an
/// error, and a session that names none gets the server's own — so a test can
/// assert what a posture *does* without a container in the way. See
/// `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
fn agency_factory() -> luu::serve::AgencyFactory {
    agency_factory_rooted(std::env::current_dir().expect("the working directory"))
}

/// Where a posture would run, as the picker shows it: the policy file's
/// `[worker]` block and nothing else, like the binary's own.
fn describe_factory() -> luu::serve::DescribeFactory {
    Arc::new(
        |policy: Option<std::path::PathBuf>, runtime: Option<agent_core::worker::Runtime>| {
            let path = policy.unwrap_or_else(|| "luu.toml".into());
            let config = agent_core::worker::WorkerConfig::from_file(&path).unwrap_or_default();
            luu::serve::Where {
                policy: path.display().to_string(),
                runtime: runtime.unwrap_or(config.runtime).as_str().to_string(),
                image: config.image,
                missing: None,
                gap: None,
            }
        },
    )
}

/// The same, over a tree the caller names.
///
/// A resume resolves a **fresh** agency rather than keeping the one the server
/// was built with, so a test whose server selects over a scratch tree has to
/// say so twice — here and in `ServeOptions`. Found by the resume warning
/// naming a file that was right there: the session came back looking for
/// `window.rs` under this package instead of under the tree it had read it
/// from.
fn agency_factory_rooted(base: std::path::PathBuf) -> luu::serve::AgencyFactory {
    // The runtime is not honoured here: no test builds a worker, and what the
    // routes decide about a variant is decided before this is called.
    Arc::new(move |policy: Option<std::path::PathBuf>, _runtime| {
        let base = base.clone();
        Box::pin(async move {
            let policy = match policy {
                Some(path) => SandboxPolicy::from_file(&path)?,
                None => SandboxPolicy::default(),
            };
            Ok(Agency::new(
                Arc::new(Tools::standard()),
                Arc::new(Sandbox::new(&policy, &base)?),
                agent_core::agent::Limits::default().with_max_steps(4),
                None,
            ))
        })
    })
}

/// A server that caches its fold into the store at `path`, so a second one
/// pointed at the same file can be asked what the first one did.
async fn server_storing(replies: Vec<String>, path: &std::path::Path) -> String {
    server_storing_selecting(
        replies,
        path,
        0,
        &std::env::current_dir().expect("the working directory"),
    )
    .await
}

/// The same, with relevance selection on over a tree the caller names.
///
/// Its own entry point rather than a parameter on every caller: exactly one
/// test needs a session whose turns carry spans, because `serve` has no
/// `--fragment` and selection is the only way code gets into a turn here.
///
/// **`base` is a parameter and not this package's directory**, which is what
/// every other server here uses. Selection walks the sandbox root and parses
/// what it finds, and `crates/luu` is two files of five thousand lines: the
/// first version of this pointed a selecting server at it and the turn after
/// an approval did not answer inside ten seconds. A tree of three small files
/// is the same code path and a test that finishes.
async fn server_storing_selecting(
    replies: Vec<String>,
    path: &std::path::Path,
    select_tokens: u32,
    base: &std::path::Path,
) -> String {
    let base = base.to_path_buf();
    let agency = Agency::new(
        Arc::new(Tools::standard()),
        Arc::new(Sandbox::new(&SandboxPolicy::default(), &base).expect("the sandbox")),
        agent_core::agent::Limits::default().with_max_steps(4),
        None,
    );
    let serving = bind(ServeOptions {
        // No icon theme in a test: the page is not what is under test.
        icons: std::sync::Arc::new(luu::icons::Theme::default()),
        // `named`: a destination handed to the server, which it keeps. One
        // nobody chose would follow the `config.toml` of whoever runs the
        // tests — and start the engine it names.
        provider: luu::provider::Resolved {
            named: true,
            ..luu::provider::Resolved::mock()
        },
        counter_warning: None,
        approvers: Default::default(),
        address: "127.0.0.1:0".parse().expect("a loopback address"),
        backend: Arc::new(Mock::replies(replies).delay(Duration::ZERO)),
        model: "mock".into(),
        record: None,
        budget: Budget::new(0, 0, Eviction::Turn),
        counter: Arc::new(ApproximateCounter),
        tokenizer: None,
        agency,
        temperature: None,
        seed: None,
        map_tokens: 0,
        map_order: Default::default(),
        map_fill: Default::default(),
        select_tokens,
        select_weights: Default::default(),
        constrain: None,
        auth_token_file: None,
        store: Some(path.to_path_buf()),
        agency_for: Some(agency_factory_rooted(base.clone())),
        describe_for: None,
        postures: Default::default(),
        postures_path: None,
    })
    .await
    .expect("binding the server");
    let address = serving.address();
    tokio::spawn(serving.run());
    address.to_string()
}

/// A server that will only take a signed approval, and the key that can make
/// one. Loopback, so nothing here is testing the token — what is being asserted
/// is that the signature is checked on its own account.
async fn server_signing() -> (String, Signer) {
    let signer = Signer::generate().expect("a key");
    let approvers = Approvers {
        required: true,
        keys: vec![ApproverKey {
            name: "jgermade".into(),
            public: signer.public(),
        }],
    };
    let address = server_everything(
        vec![PLAN.into(), ANSWER.into()],
        Duration::ZERO,
        SandboxPolicy::default(),
        Budget::new(0, 0, Eviction::Turn),
        None,
        approvers,
    )
    .await;
    (address, signer)
}

async fn server_authed(
    replies: Vec<String>,
    delay: Duration,
    policy: SandboxPolicy,
    budget: Budget,
    auth_token_file: Option<std::path::PathBuf>,
) -> String {
    server_everything(
        replies,
        delay,
        policy,
        budget,
        auth_token_file,
        Approvers::default(),
    )
    .await
}

async fn server_everything(
    replies: Vec<String>,
    delay: Duration,
    policy: SandboxPolicy,
    budget: Budget,
    auth_token_file: Option<std::path::PathBuf>,
    approvers: Approvers,
) -> String {
    server_with_postures(
        replies,
        delay,
        policy,
        budget,
        auth_token_file,
        approvers,
        Default::default(),
        None,
        None,
    )
    .await
}

/// The same, with the postures a session may be started on.
///
/// Handed in rather than read from `config.toml`, for the reason the server
/// resolves them once at startup: a test that set `LUU_HOME` would be changing
/// a process-wide answer that every other test in this binary reads.
#[allow(clippy::too_many_arguments)]
async fn server_with_postures(
    replies: Vec<String>,
    delay: Duration,
    policy: SandboxPolicy,
    budget: Budget,
    auth_token_file: Option<std::path::PathBuf>,
    approvers: Approvers,
    postures: std::collections::BTreeMap<String, luu::provider::Posture>,
    // A store, for the tests that need a session to outlive the one that is
    // live — which is every test about what a *resume* is allowed to do.
    store: Option<std::path::PathBuf>,
    // And a `--record` file, for the one test about what a *file* says when it
    // holds more than one session.
    record: Option<std::path::PathBuf>,
) -> String {
    let base = std::env::current_dir().expect("the working directory");
    let agency = Agency::new(
        Arc::new(Tools::standard()),
        Arc::new(Sandbox::new(&policy, &base).expect("the sandbox")),
        agent_core::agent::Limits::default().with_max_steps(4),
        None,
    );
    let serving = bind(ServeOptions {
        // No icon theme in a test: the page is not what is under test.
        icons: std::sync::Arc::new(luu::icons::Theme::default()),
        // `named`: a destination handed to the server, which it keeps. One
        // nobody chose would follow the `config.toml` of whoever runs the
        // tests — and start the engine it names.
        provider: luu::provider::Resolved {
            named: true,
            ..luu::provider::Resolved::mock()
        },
        counter_warning: None,
        approvers,
        address: "127.0.0.1:0".parse().expect("a loopback address"),
        backend: Arc::new(Mock::replies(replies).delay(delay)),
        model: "mock".into(),
        record,
        budget,
        counter: Arc::new(ApproximateCounter),
        tokenizer: None,
        agency,
        temperature: None,
        seed: None,
        map_tokens: 0,
        map_order: Default::default(),
        map_fill: Default::default(),
        select_tokens: 0,
        select_weights: Default::default(),
        constrain: None,
        auth_token_file,
        store,
        agency_for: Some(agency_factory()),
        describe_for: Some(describe_factory()),
        postures,
        postures_path: Some("config.toml".into()),
    })
    .await
    .expect("binding the server");

    let address = serving.address();
    tokio::spawn(serving.run());
    address.to_string()
}

async fn next_message(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(PATIENCE, socket.next())
        .await
        .expect("the server went quiet")
        .expect("the socket closed")
        .expect("a frame");
    match frame {
        WsMessage::Text(text) => serde_json::from_str(&text).expect("a protocol message"),
        other => panic!("expected text, got {other:?}"),
    }
}

/// Reads until the named message arrives, collecting the tokens on the way.
/// The transcript is what the assertions are about; the order tokens arrive in
/// relative to each other is `run_turn`'s business and is tested there.
async fn until(socket: &mut Socket, kind: &str) -> (Value, String) {
    let mut text = String::new();
    loop {
        let message = next_message(socket).await;
        match message["type"].as_str().expect("a typed message") {
            "token" => text.push_str(message["text"].as_str().unwrap_or_default()),
            found if found == kind => return (message, text),
            _ => continue,
        }
    }
}

async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(WsMessage::Text(message.to_string().into()))
        .await
        .expect("sending");
}

async fn get(address: &str, path: &str) -> Value {
    reqwest::get(format!("http://{address}{path}"))
        .await
        .expect("the request")
        .json()
        .await
        .expect("a JSON body")
}

async fn post(address: &str, path: &str) -> reqwest::Response {
    let client = reqwest::Client::new();
    client
        .post(format!("http://{address}{path}"))
        .send()
        .await
        .expect("the request")
}

async fn patch(address: &str, path: &str, body: Value) -> reqwest::Response {
    let client = reqwest::Client::new();
    client
        .patch(format!("http://{address}{path}"))
        .json(&body)
        .send()
        .await
        .expect("the request")
}

async fn delete(address: &str, path: &str) -> reqwest::Response {
    let client = reqwest::Client::new();
    client
        .delete(format!("http://{address}{path}"))
        .send()
        .await
        .expect("the request")
}

#[tokio::test]
async fn a_prompt_is_planned_approved_and_answered_over_the_socket() {
    let address = server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");

    let hello = next_message(&mut socket).await;
    assert_eq!(hello["type"], "hello");
    // 6 since `grounded`, 5 since the handshake, 4 since `jobs`, 3 since
    // `evicted`, 2 since `refused`: a new variant of a tagged enum is a change
    // an older reader cannot parse, which is what this number is for. The
    // constant and not the literal, for the reason
    // `a_matching_client_is_greeted_and_then_ignored` gives one screen down: a
    // number typed here makes every bump look like a broken handshake.
    assert_eq!(hello["protocol"], agent_core::protocol::VERSION);
    assert_eq!(hello["backend"], "mock");
    assert!(hello["turn"].is_null(), "nothing is running yet");
    assert!(
        hello["session"].is_string(),
        "what an approval is signed against, so a signature does not replay elsewhere",
    );

    // No gate in front of it any more: the prompt runs, in the draft it opens.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;

    let (started, _) = until(&mut socket, "turn_started").await;
    assert_eq!(started["turn"], 1);
    assert_eq!(started["prompt"], "add a flag");

    let (ended, drafting) = until(&mut socket, "ended").await;
    assert_eq!(ended["turn"], 1);
    assert_eq!(ended["reason"], "stop");
    assert_eq!(drafting, PLAN, "the drafting turn's own text");

    let (drafted, _) = until(&mut socket, "draft_opened").await;
    assert_eq!(
        drafted["job"], 1,
        "every turn belongs to a job, from the first"
    );
    assert_eq!(
        drafted["objective"], "add a flag",
        "a draft's objective is what the user asked, as they asked it",
    );

    // The model's own door: its answer carried a plan block, so it reaches the
    // gate as a suggestion. It names no job, because an id is what approval
    // hands out and nobody has approved anything.
    let (proposed, _) = until(&mut socket, "plan_proposed").await;
    assert!(
        proposed["job"].is_null(),
        "a proposal is not a job: {proposed}"
    );
    assert_eq!(proposed["objective"], "add a flag");
    assert_eq!(proposed["plan"]["files"][0], "Cargo.toml");
    assert_eq!(proposed["source"], "model");

    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;

    // Approving *is* closing, and the two halves arrive in the order a client
    // has to draw them: the draft folds, then the job it opened.
    let (closed, _) = until(&mut socket, "job_closed").await;
    assert_eq!(closed["job"], 1);
    assert!(
        closed["summary"]
            .as_str()
            .expect("a summary")
            .starts_with("[draft closed]"),
        "{closed}",
    );

    let (approved, _) = until(&mut socket, "job_approved").await;
    assert_eq!(approved["job"], 2, "the approval handed out the id");
    assert_eq!(approved["from"], 1, "and named the draft it closed");
    assert_eq!(approved["objective"], "add a flag");

    // No held prompt follows it. The work starts on the next prompt, which
    // lands in the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;

    let (started, _) = until(&mut socket, "turn_started").await;
    assert_eq!(started["turn"], 2);
    assert_eq!(started["job"], 2, "inside the job that was approved");

    let (ended, answer) = until(&mut socket, "ended").await;
    assert_eq!(ended["turn"], 2);
    assert_eq!(answer, ANSWER);
    assert!(ended["usage"]["completion_tokens"].as_u64().unwrap_or(0) > 0);

    // The read side, folded from the events just watched. It is served by the
    // same process over the same port, so this is the live API, not an export.
    let sessions = get(&address, "/api/sessions").await;
    assert_eq!(sessions.as_array().expect("an array").len(), 1);
    assert_eq!(sessions[0]["id"], "live");
    assert_eq!(sessions[0]["backend"], "mock");
    assert_eq!(sessions[0]["turns"], 2);

    let turns = get(&address, "/api/sessions/live/turns").await;
    assert_eq!(turns[0]["turn"], 1);
    assert_eq!(turns[0]["text"], PLAN);
    assert_eq!(turns[0]["job"], 1, "the drafting turn belongs to the draft");
    assert_eq!(turns[1]["turn"], 2);
    assert_eq!(turns[1]["text"], ANSWER);
    assert_eq!(turns[1]["job"], 2);
    assert_eq!(
        turns[1]["reason"], "stop",
        "the read side agrees with the `ended` the socket carried",
    );

    // Both spellings answer, because a static host can only mirror one of them.
    let suffixed = get(&address, "/api/sessions/live/turns.json").await;
    assert_eq!(suffixed, turns);

    let prompt = get(&address, "/api/sessions/live/turns/1/prompt").await;
    let drafting_prompt = prompt["text"].as_str().expect("the prompt as sent");
    assert!(
        drafting_prompt.contains(SYSTEM),
        "the system block is the prefix every call shares: {drafting_prompt}",
    );
    assert!(
        !drafting_prompt.contains("propose a plan"),
        "a drafting turn is an ordinary turn: {drafting_prompt}",
    );

    // The session, as the alternation leaves it: a folded draft and the job it
    // became, and one round at the gate that was approved.
    let session = get(&address, "/api/sessions/live").await;
    let jobs = session["jobs"].as_array().expect("the jobs");
    assert_eq!(jobs.len(), 2);
    assert!(jobs[0]["plan"].is_null(), "job 1 is the draft: {}", jobs[0]);
    assert_eq!(jobs[1]["plan"]["files"][0], "Cargo.toml");
    let rounds = session["rounds"].as_array().expect("the rounds");
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0]["answer"]["answer"], "approved");
    assert_eq!(rounds[0]["answer"]["job"], 2);
}

/// The other door, and what a plan is asked *over*: the draft, not a held
/// prompt. The planning instruction is fused into the call that asks for it and
/// is paid for nowhere else.
#[tokio::test]
async fn a_plan_can_be_asked_for_and_is_planned_over_the_draft() {
    let address = server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    let _ = next_message(&mut socket).await;

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "have a look"}),
    )
    .await;
    until(&mut socket, "draft_opened").await;
    // The mock's first reply is a plan block, so the drafting turn already
    // suggested one. Take it off the table, which ends nothing.
    until(&mut socket, "plan_proposed").await;
    send(&mut socket, serde_json::json!({"type": "decline_plan"})).await;
    until(&mut socket, "plan_declined").await;

    send(&mut socket, serde_json::json!({"type": "request_plan"})).await;
    let (started, _) = until(&mut socket, "turn_started").await;
    assert_eq!(started["turn"], 2, "the planning call is a turn");

    until(&mut socket, "ended").await;

    let prompt = get(&address, "/api/sessions/live/turns/2/prompt").await;
    let planning = prompt["text"].as_str().expect("the prompt as sent");
    assert!(
        planning.contains("propose a plan"),
        "the planning instruction is fused into the user message: {planning}",
    );
    assert!(
        planning.contains("The conversation above is the draft"),
        "and what it is asked over is the draft: {planning}",
    );

    // The planning call is a turn — it costs a window and every panel explains
    // it — so the read side has two. What it is not is *remembered*: it never
    // enters the context, so the draft it planned over still holds one turn.
    let turns = get(&address, "/api/sessions/live/turns").await;
    assert_eq!(turns.as_array().expect("the turns").len(), 2);
    assert_eq!(
        turns[1]["job"], 1,
        "the planning call is asked inside the draft it compacts — what it is \
         not is remembered in it",
    );

    // And the decline is kept, because how many rounds a plan takes is the
    // number that event exists to produce.
    let session = get(&address, "/api/sessions/live").await;
    let rounds = session["rounds"].as_array().expect("the rounds");
    assert_eq!(rounds[0]["answer"]["answer"], "declined");
}

/// A client that says something the protocol does not define must not take the
/// server down for the others — the socket stays open and the next message is
/// still answered.
#[tokio::test]
async fn unparseable_input_does_not_kill_the_socket() {
    let address = server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(&mut socket, serde_json::json!({"type": "explode"})).await;
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;

    let (started, _) = until(&mut socket, "turn_started").await;
    assert_eq!(started["turn"], 1);
}

/// A path nobody serves is a 404 and not a panic, and the UI's own index is
/// served from the embedded assets rather than from a directory that only
/// exists in a checkout.
#[tokio::test]
async fn the_page_and_the_missing_page() {
    let address = server().await;

    let index = reqwest::get(format!("http://{address}/"))
        .await
        .expect("the request");
    assert!(index.status().is_success());
    let body = index.text().await.expect("the body");
    assert!(body.contains('<'), "the index is not empty");

    let missing = reqwest::get(format!("http://{address}/api/sessions/nope"))
        .await
        .expect("the request");
    assert_eq!(missing.status(), 404);
}

/// The terminal opens where the session's commands run, and only for this
/// server's page.
///
/// This server runs its tools on the host and is bound on loopback, so the
/// default `[terminal] host = "loopback"` offers a shell here — opened, typed
/// into, and read back. An upgrade from any other origin, or none, is refused
/// before anything is asked: a WebSocket is not covered by CORS, so `Origin`
/// is the only thing between a site in another tab and a shell. See
/// `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
#[tokio::test]
async fn the_terminal_opens_where_the_session_runs_and_only_for_this_page() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let address = server().await;

    let view = get(&address, "/api/terminal").await;
    assert_eq!(view["available"], true, "{view}");
    assert_eq!(view["place"], "host", "{view}");

    let upgrade = |origin: Option<String>| {
        let mut request = reqwest::Client::new()
            .get(format!("http://{address}/ws/terminal"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        request.send()
    };
    let other = upgrade(Some("https://example.com".into()))
        .await
        .expect("sent");
    assert_eq!(other.status(), 403);
    assert!(other.text().await.unwrap().contains("Origin"));
    let none = upgrade(None).await.expect("sent");
    assert_eq!(none.status(), 403);

    // Its own page: a shell, on a PTY, that does what it is told.
    let mut request = format!("ws://{address}/ws/terminal")
        .into_client_request()
        .expect("a request");
    request.headers_mut().insert(
        "Origin",
        format!("http://{address}").parse().expect("a header"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the terminal's handshake");
    // Arithmetic, so the line that comes back is the shell's and not the echo
    // of what was typed.
    socket
        .send(WsMessage::Binary(b"echo LUU_$((40+2))\n".to_vec().into()))
        .await
        .expect("typed");
    let mut seen = String::new();
    let found = tokio::time::timeout(PATIENCE, async {
        while let Some(Ok(frame)) = socket.next().await {
            if let WsMessage::Binary(bytes) = frame {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if seen.contains("LUU_42") {
                    return true;
                }
            }
        }
        false
    })
    .await;
    assert!(
        matches!(found, Ok(true)),
        "the shell never answered: {seen:?}"
    );
    let _ = socket.close(None).await;
}

/// The live session moves to another posture between jobs, and never inside
/// one.
///
/// What went wrong when a posture moved under a session was something
/// approved under one carried into another — so a proposal at the gate and an
/// approved job are refusals, and a session with neither moves and says where
/// it went. See `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
#[tokio::test]
async fn the_live_session_moves_between_postures_between_jobs() {
    let dir = std::env::temp_dir().join(format!("luu-move-posture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory for the policy file");
    let policy = dir.join("wide.toml");
    std::fs::write(
        &policy,
        "[sandbox]\nnetwork = true\n\n[[sandbox.paths]]\npath = \".\"\naccess = \"read-write\"\n",
    )
    .expect("a policy file");
    // A contained posture too, for its variants. The test factory never
    // starts its worker; only what the routes decide is under test.
    let boxed = dir.join("boxed.toml");
    std::fs::write(
        &boxed,
        "[worker]\nruntime = \"podman\"\nimage = \"luu-worker:dev\"\n",
    )
    .expect("a policy file");
    let mut postures = std::collections::BTreeMap::new();
    postures.insert(
        "wide".to_string(),
        luu::provider::Posture {
            policy: policy.clone(),
        },
    );
    postures.insert(
        "boxed".to_string(),
        luu::provider::Posture { policy: boxed },
    );
    let address = server_with_postures(
        vec![PLAN.into(), ANSWER.into()],
        Duration::ZERO,
        SandboxPolicy::default(),
        Budget::new(0, 0, Eviction::Turn),
        None,
        Approvers::default(),
        postures,
        None,
        None,
    )
    .await;
    let client = reqwest::Client::new();
    let mv = |posture: Value| {
        client
            .put(format!("http://{address}/api/session/posture"))
            .json(&serde_json::json!({ "posture": posture }))
            .send()
    };

    // Each posture is named by where it would run, read from its file
    // without starting anything — the terminal's picker.
    let offered = get(&address, "/api/postures").await;
    assert_eq!(offered["places"]["wide"][0]["runtime"], "host", "{offered}");
    // A host posture has no variants: its paths are on this machine.
    assert_eq!(
        offered["places"]["wide"].as_array().map(Vec::len),
        Some(1),
        "{offered}"
    );
    assert!(offered["own"][0]["runtime"].is_string(), "{offered}");
    // A contained one is offered on every container runtime, its own first.
    let boxed = &offered["places"]["boxed"];
    let runtimes: Vec<&str> = boxed
        .as_array()
        .expect("variants")
        .iter()
        .filter_map(|place| place["runtime"].as_str())
        .collect();
    assert_eq!(
        runtimes,
        ["podman", "docker", "nerdctl", "colima", "container"],
        "{offered}"
    );

    // A variant is a word out of five, and only between containers.
    let vary = |posture: &str, runtime: &str| {
        client
            .put(format!("http://{address}/api/session/posture"))
            .json(&serde_json::json!({ "posture": posture, "runtime": runtime }))
            .send()
    };
    let to_host = vary("boxed", "host").await.expect("sent");
    assert_eq!(to_host.status(), 400);
    assert!(
        to_host
            .text()
            .await
            .unwrap()
            .contains("not a container runtime")
    );
    let host_boxed = vary("wide", "docker").await.expect("sent");
    assert_eq!(host_boxed.status(), 400);
    assert!(
        host_boxed
            .text()
            .await
            .unwrap()
            .contains("only a contained posture")
    );
    assert_eq!(vary("boxed", "lxc").await.expect("sent").status(), 400);

    let moved = mv("wide".into()).await.expect("sent");
    assert_eq!(moved.status(), 200);
    assert_eq!(moved.json::<Value>().await.unwrap()["moved"], true);
    let settings = get(&address, "/api/settings").await;
    assert_eq!(settings["posture"]["name"], "wide");
    assert_eq!(settings["posture"]["network"], true, "{settings}");
    // The fold says so too, which is what a store and an export read.
    let sessions = get(&address, "/api/sessions/live").await;
    assert_eq!(sessions["posture"]["name"], "wide", "{sessions}");

    // Where it already is: nothing to write.
    let again = mv("wide".into()).await.expect("sent");
    assert_eq!(again.json::<Value>().await.unwrap()["moved"], false);
    // A name the file does not have is refused, and moves nothing.
    assert_eq!(mv("nope".into()).await.expect("sent").status(), 400);
    // And back to the server's own.
    assert_eq!(mv(Value::Null).await.expect("sent").status(), 200);
    assert_eq!(
        get(&address, "/api/settings").await["posture"]["network"],
        false
    );

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    let at_the_gate = mv("wide".into()).await.expect("sent");
    assert_eq!(at_the_gate.status(), 409);
    assert!(at_the_gate.text().await.unwrap().contains("gate"));

    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    let (approved, _) = until(&mut socket, "job_approved").await;
    let inside = mv("wide".into()).await.expect("sent");
    assert_eq!(inside.status(), 409);
    let said = inside.text().await.unwrap();
    assert!(said.contains(&format!("job {}", approved["job"])), "{said}");
    // Refused means unmoved.
    assert_eq!(
        get(&address, "/api/settings").await["posture"]["network"],
        false
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The token gates authority and the read side, and does not gate the page.
///
/// Three surfaces, three answers: `/ws` carries `approve_task` and is refused
/// without the token, `/api/*` carries this session's prompts and is refused
/// the same way, and the embedded UI is served to anyone — it is the same
/// bytes in every copy of the binary, and a browser cannot put a header on a
/// navigation.
#[tokio::test]
async fn a_bearer_token_gates_the_socket_and_the_read_side() {
    let (address, dir) = server_guarded("s3cret").await;

    let anonymous = reqwest::get(format!("http://{address}/api/sessions"))
        .await
        .expect("the request");
    assert_eq!(anonymous.status(), 401);
    assert_eq!(
        anonymous
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer"),
    );

    let bearer = reqwest::Client::new()
        .get(format!("http://{address}/api/sessions"))
        .bearer_auth("s3cret")
        .send()
        .await
        .expect("the request");
    assert!(bearer.status().is_success(), "{:?}", bearer.status());

    // The page: open, and the only thing that is.
    let index = reqwest::get(format!("http://{address}/"))
        .await
        .expect("the request");
    assert!(index.status().is_success());

    // The socket, both ways round. The browser concession is the query
    // parameter, so that is what the passing half uses.
    tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect_err("an unauthenticated upgrade");
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/ws?token=s3cret"))
            .await
            .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    std::fs::remove_dir_all(&dir).ok();
}

/// Narrowing, over the socket: the plan names `Cargo.toml`, the turn asks for
/// `src/serve.rs`, and the sandbox that refuses is the plan rather than the
/// policy file — which still grants it.
#[tokio::test]
async fn a_turn_may_not_touch_what_its_task_was_not_approved_for() {
    let address = server_with(vec![
        PLAN_FOR_CARGO_TOML.into(),
        READS_SERVE_RS.into(),
        "I was not approved to read that.".into(),
    ])
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "read the manifest"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;

    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(result["name"], "read_file");
    assert_eq!(result["verdict"]["allowed"], false);
    assert!(
        result["verdict"]["rule"]
            .as_str()
            .expect("a rule")
            .contains("the approved plan for job 2"),
        "a denial has to say which authority refused: {}",
        result["verdict"]["rule"],
    );

    // The same file, under the policy file the job narrowed: still granted.
    // The refusal is the job's, not the session's.
    let sandbox = Sandbox::new(
        &SandboxPolicy::default(),
        &std::env::current_dir().expect("the working directory"),
    )
    .expect("the sandbox");
    assert!(
        sandbox
            .check_path(std::path::Path::new("src/serve.rs"), Access::Read)
            .verdict
            .allowed,
    );
}

/// The other half, which is what makes narrowing survivable: the person at the
/// gate adds the file the plan forgot, and the turn goes through.
#[tokio::test]
async fn a_file_added_at_the_gate_is_in_the_task_sandbox() {
    let address = server_with(vec![
        PLAN_FOR_CARGO_TOML.into(),
        READS_SERVE_RS.into(),
        "Read it.".into(),
    ])
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "read the manifest"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;

    // Approving *with* an amendment, plus one entry the policy file does not
    // grant: the gate widens a plan up to the file and not past it.
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "files": ["src/serve.rs", "/etc/passwd"],
            "commands": [],
        }),
    )
    .await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;

    let (approved, _) = until(&mut socket, "job_approved").await;
    let files = approved["plan"]["files"]
        .as_array()
        .expect("the plan as approved");
    assert_eq!(files, &["Cargo.toml", "src/serve.rs"]);
    assert!(
        !files.iter().any(|file| file == "/etc/passwd"),
        "what the policy file does not grant is not approved by a person asking for it",
    );

    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(result["name"], "read_file");
    assert_eq!(
        result["verdict"]["allowed"], true,
        "{}",
        result["verdict"]["rule"],
    );

    // And the read API carries the plan as approved, not as proposed.
    let session = get(&address, "/api/sessions/live").await;
    assert_eq!(session["jobs"][1]["plan"]["files"][1], "src/serve.rs");
}

/// A reopen the alternation put out of reach says *that*, and not something
/// false.
///
/// Row 33. `Context::reopen_job` takes the **last** job and no other, which is
/// argued where it is written: reopening an earlier one either leaves two open
/// or obliges the reopen to close what came after. Item 22 is what made the
/// rule bite — every prompt opens a draft, so the next prompt is what ends the
/// window — and until this the refusal said *job N is not closed* about a job
/// the person had just watched fold.
///
/// The page stopped offering the button here at the same time, which is the
/// half a browser test asserts; this is the half that matters if somebody
/// reaches the route another way. See
/// `RECORD/2026-09-21.the-alternation-on-the-page.completed.md`.
#[tokio::test]
async fn a_reopen_of_anything_but_the_last_job_says_which() {
    // Two replies for two turns: the first answers with a plan block, which is
    // what puts the gate up, and the second is the prompt that opens a draft
    // after the job is folded. **Approving runs nothing** — under the
    // alternation the prompt that proposed is already answered, and a turn
    // waiting behind the gate is what item 22 removed.
    let address = server_with(vec![PLAN.into(), ANSWER.into()]).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    // A plan, approved: draft 1 closes and job 2 opens, which is the whole
    // alternation in one message pair.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    let (approved, _) = until(&mut socket, "job_approved").await;
    let job = approved["job"].as_u64().expect("the job it opened");

    // Fold it, and then open a draft after it with one more prompt. Without
    // that prompt the reopen would succeed — which is the window item 22 left.
    send(
        &mut socket,
        serde_json::json!({"type": "close_job", "job": job}),
    )
    .await;
    until(&mut socket, "job_closed").await;
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "anything else?"}),
    )
    .await;
    until(&mut socket, "ended").await;
    let (opened, _) = until(&mut socket, "draft_opened").await;
    let draft = opened["job"].as_u64().expect("the draft after it");

    send(
        &mut socket,
        serde_json::json!({"type": "reopen_job", "job": job}),
    )
    .await;
    let (refused, _) = until(&mut socket, "refused").await;
    assert_eq!(refused["request"], "reopen_job");
    assert_eq!(refused["reason"], "job", "{refused}");
    let detail = refused["detail"].as_str().expect("a detail");
    assert!(
        detail.contains("not the last one") && detail.contains(&draft.to_string()),
        "the refusal names the job that came after it, because that is what a \
         person can act on: {detail}",
    );
    assert!(
        !detail.contains("is not closed"),
        "and does not say the opposite of what the transcript shows: {detail}",
    );
}

/// The three other silences, each of which used to be an early return.
#[tokio::test]
async fn the_server_says_why_it_did_not_do_something() {
    // A slow mock, so the second prompt lands while the first turn is still
    // running rather than after it — which is the state being tested.
    let address = server_with(vec![PLAN.into(), ANSWER.into()]).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    // Nothing is open, so nothing can be closed or reopened. These two still
    // name a job, because they are the two that still act on one.
    for request in ["close_job", "reopen_job"] {
        send(&mut socket, serde_json::json!({"type": request, "job": 7})).await;
        let (refused, _) = until(&mut socket, "refused").await;
        assert_eq!(refused["request"], request);
        assert_eq!(refused["reason"], "job", "{refused}");
        assert!(
            refused["detail"].as_str().expect("a detail").contains("7"),
            "a refusal names what was refused: {refused}",
        );
    }

    // And nothing is on the table, so there is nothing to answer. The two gate
    // messages name no job — there is none to name until one is approved — so
    // what they are refused *about* is the table being empty. The older
    // spellings reach the same handlers, which is what the aliases are for.
    for (sent, request) in [
        ("approve_plan", "approve_plan"),
        ("decline_plan", "decline_plan"),
        ("approve_job", "approve_plan"),
        ("reject_job", "decline_plan"),
    ] {
        send(&mut socket, serde_json::json!({"type": sent, "job": 7})).await;
        let (refused, _) = until(&mut socket, "refused").await;
        assert_eq!(refused["request"], request, "{refused}");
        assert_eq!(refused["reason"], "job", "{refused}");
        assert!(
            refused["detail"]
                .as_str()
                .expect("a detail")
                .contains("no plan is on the table"),
            "a refusal says what was refused: {refused}",
        );
    }
}

/// A prompt sent while a turn is running: the one the `busy` item was named
/// after. The planning call is still streaming when the second prompt arrives.
#[tokio::test]
async fn a_prompt_while_a_turn_runs_is_refused_as_busy() {
    let address = server_slow(vec![PLAN.into(), ANSWER.into()]).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    // The turn has started and its tokens are arriving one every 30ms, so this
    // arrives in the middle of it.
    until(&mut socket, "turn_started").await;
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "and this too"}),
    )
    .await;

    let (refused, _) = until(&mut socket, "refused").await;
    assert_eq!(refused["request"], "prompt");
    assert_eq!(refused["reason"], "busy");
    assert!(
        refused["detail"]
            .as_str()
            .expect("a detail")
            .contains("turn 1"),
        "{refused}",
    );
}

/// The fourth one, and the newest: an amendment at the gate naming something
/// the policy file does not grant is dropped from the plan — and now says so.
#[tokio::test]
async fn an_amendment_the_policy_refuses_is_reported_not_only_dropped() {
    let address = server_with(vec![
        PLAN_FOR_CARGO_TOML.into(),
        "Read it.".into(),
        "Read it.".into(),
    ])
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "read the manifest"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "files": ["/etc/passwd"],
            "commands": [],
        }),
    )
    .await;

    let (refused, _) = until(&mut socket, "refused").await;
    assert_eq!(refused["request"], "approve_plan");
    assert_eq!(refused["reason"], "not_granted");
    assert!(
        refused["detail"]
            .as_str()
            .expect("a detail")
            .contains("/etc/passwd"),
        "{refused}",
    );

    // And the job still runs: the approval was not thrown away with the part
    // of it nobody may grant.
    let (approved, _) = until(&mut socket, "job_approved").await;
    assert_eq!(approved["plan"]["files"], serde_json::json!(["Cargo.toml"]));
}

/// The refusal reaches the tally, which is the whole of why the fold keeps
/// refusals at all.
///
/// `not_granted` is the only refusal in the protocol that stops nothing and the
/// only place a person learns the policy file has a floor —
/// `RECORD/2026-09-17.the-gate-panel-narrows.completed.md` found it being
/// cleared from the page within a frame of arriving. It was also being dropped
/// by the fold, under a comment reserving the right to keep it *until something
/// wants to count them*. Driven end to end rather than folded from a fixture,
/// because what is being checked is that a real refusal off a real gate reaches
/// a route, and both halves of that had been true separately for weeks.
#[tokio::test]
async fn a_refusal_the_page_clears_in_a_frame_is_still_in_the_count() {
    let address = server_with(vec![
        PLAN_FOR_CARGO_TOML.into(),
        "Read it.".into(),
        "Read it.".into(),
    ])
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    assert_eq!(
        get(&address, "/api/sessions/live/counts").await["refused"],
        serde_json::json!([]),
        "nothing refused yet, and an empty list rather than no field",
    );

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "read the manifest"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "files": ["/etc/passwd"],
            "commands": [],
        }),
    )
    .await;
    until(&mut socket, "refused").await;
    until(&mut socket, "job_approved").await;

    let counts = get(&address, "/api/sessions/live/counts").await;
    assert_eq!(
        counts["refused"],
        serde_json::json!([{"reason": "not_granted", "count": 1}]),
    );
    assert_eq!(
        counts["jobs"]["approved"], 1,
        "and the job it did not stop is counted as approved",
    );
    // The grant is a count of path *strings*: which of them is a directory is
    // a question about a disk, and item 12 of ROADMAP/2026-09-17 is taken this
    // far and no further. See RECORD/2026-09-19.one-counting-surface.completed.md.
    assert_eq!(
        counts["jobs"]["reads"], 1,
        "Cargo.toml, /etc/passwd dropped"
    );

    assert_eq!(
        get(&address, "/api/sessions/live/counts.json").await,
        counts,
        "both spellings, as every route here answers",
    );
}

/// The lifecycle is a state machine and the socket is open to anyone: closing
/// a job that was only *proposed* used to succeed, which took the gate off
/// the screen with its prompt still held and left the session with no way to
/// answer a proposal nobody could see. Found by driving the real page.
#[tokio::test]
async fn a_proposal_cannot_be_closed_out_from_under_the_gate() {
    let address = server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;

    // Closing the draft the plan was offered inside would take the gate off
    // the screen with the question still up, and the session would have
    // nothing to answer it with. The job it names exists — it is the draft —
    // which is exactly why the refusal has to be about the plan on the table
    // rather than about the job being absent.
    send(
        &mut socket,
        serde_json::json!({"type": "close_job", "job": 1}),
    )
    .await;
    let (refused, _) = until(&mut socket, "refused").await;
    assert_eq!(refused["request"], "close_job");
    assert_eq!(refused["reason"], "pending", "{refused}");

    // Still waiting on a person, and still answerable.
    let session = get(&address, "/api/sessions/live").await;
    assert_eq!(session["jobs"][0]["state"], "open");
    assert!(
        session["jobs"][0]["plan"].is_null(),
        "it is still the draft"
    );

    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    let (approved, _) = until(&mut socket, "job_approved").await;
    assert_eq!(approved["job"], 2, "the question was still answerable");
}

/// Narrowing on level, over the socket: the plan says it will *read* the
/// manifest, so writing it is refused by the task's own plan — and the policy
/// file, which grants read-write on the tree, is not what refused.
#[tokio::test]
async fn a_turn_may_not_write_a_file_its_plan_only_reads() {
    let address = server_with(vec![
        PLAN_FOR_CARGO_TOML.into(),
        WRITES_SCRATCH.into(),
        "I was not approved to change that.".into(),
    ])
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "read the manifest"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;

    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(result["name"], "write_file");
    assert_eq!(result["verdict"]["allowed"], false);
    assert!(
        result["verdict"]["rule"]
            .as_str()
            .expect("a rule")
            .contains("the approved plan for job 2"),
        "{}",
        result["verdict"]["rule"],
    );

    // The manifest is untouched, which is the only assertion that would notice
    // a check that ran after the write rather than before it.
    let manifest = std::fs::read_to_string("Cargo.toml").expect("the manifest");
    assert!(
        manifest.contains("[package]"),
        "the file was written anyway"
    );
}

/// The floor, which is the same assertion one job earlier: **a draft turn may
/// not write what the policy file grants.**
///
/// The sibling above narrows an approved job against its plan. This one has no
/// plan to narrow against — it is the first prompt of a session, so it runs in
/// a draft — and the thing that refuses is the floor. The policy file here is
/// `SandboxPolicy::default()`, read-write on the tree, and between item 22 and
/// `RECORD/2026-09-21.the-drafts-floor.completed.md` this write went through.
#[tokio::test]
async fn a_draft_turn_may_not_write_what_the_policy_file_grants() {
    // A path **outside the checkout** that the policy file explicitly grants
    // at read-write, rather than a file in the tree. The distinction is not
    // fussiness: this test's failure mode *is* a successful write, so pointing
    // it at the checkout means a red run that also edits the repository. That
    // is how this test was first written and the first failing run replaced
    // `crates/luu/Cargo.toml` with the word `broken`.
    let scratch = std::env::temp_dir().join(format!("luu-draft-floor-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&scratch);
    let call = format!(
        "Writing it.\n```tool\n{{\"name\":\"write_file\",\"arguments\":         {{\"path\":{},\"content\":\"written by a draft\"}}}}\n```",
        serde_json::to_string(&scratch.display().to_string()).expect("a path"),
    );
    let address = server_writable(
        vec![call, "I am drafting, so I may not change that.".into()],
        &scratch,
    )
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    // No approval anywhere in this test, which is the whole point: the turn
    // runs in the draft the prompt opens.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "write the scratch file"}),
    )
    .await;

    // The floor still refuses; what changed is that a write the policy file
    // grants is now put to a person before the model hears about it. So the
    // floor's words arrive on the question rather than on the result. See
    // `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
    let (held, _) = until(&mut socket, "call_held").await;
    assert_eq!(held["name"], "write_file");
    assert!(
        held["refused"]
            .as_str()
            .expect("a rule")
            .contains("the draft's floor"),
        "a denial has to say which authority refused, and the floor is not the \
         policy file: {}",
        held["refused"],
    );
    send(
        &mut socket,
        serde_json::json!({"type": "answer_call", "turn": held["turn"], "step": held["step"],
                           "allow": false}),
    )
    .await;

    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(result["name"], "write_file");
    assert_eq!(result["verdict"]["allowed"], false);
    assert_eq!(result["asked"], false);

    // The only assertion that would notice a check running *after* the write,
    // and the one that says the policy file was not what refused: the same
    // path, granted read-write by the very policy this server was built with.
    assert!(
        !scratch.exists(),
        "the file was written anyway, so the floor is not holding",
    );
    let _ = std::fs::remove_file(&scratch);
}

/// And the refusal names *which* draft, one turn in.
///
/// Row 32, and it is the sibling above with one prompt in front of it. On the
/// first prompt of a session there is no draft to name — `open_draft` runs when
/// the turn lands, after the sandbox is chosen — so that one reads *the draft's
/// floor*, which is what the test above asserts. Every prompt after it has a
/// draft open in the same context `serve` already reads, and the denial
/// denormalises it the way `Authority::Plan` always has.
///
/// The job id is read from the wire rather than assumed to be 1: it is the
/// draft the first prompt opened, and a test that hard-coded it would pass on
/// the wrong number the day a session opens with anything else. See
/// `RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`.
#[tokio::test]
async fn a_refusal_in_a_draft_names_the_draft_it_was_refused_in() {
    let scratch = std::env::temp_dir().join(format!("luu-draft-named-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&scratch);
    let call = format!(
        "Writing it.\n```tool\n{{\"name\":\"write_file\",\"arguments\":         {{\"path\":{},\"content\":\"written by a draft\"}}}}\n```",
        serde_json::to_string(&scratch.display().to_string()).expect("a path"),
    );
    let address = server_writable(
        vec![
            "Still drafting.".into(),
            call,
            "I am drafting, so I may not change that.".into(),
        ],
        &scratch,
    )
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    // The first prompt, which opens the draft and asks for nothing.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "what is here"}),
    )
    .await;
    // `draft_opened` lands *after* the turn it was opened to hold — a job
    // opens when a turn lands in it — so the two waits are in that order and
    // not the reading one.
    until(&mut socket, "ended").await;
    let (opened, _) = until(&mut socket, "draft_opened").await;
    let draft = opened["job"].as_u64().expect("the draft it opened");

    // The second, inside the draft the first one opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "write the scratch file"}),
    )
    .await;
    let (held, _) = until(&mut socket, "call_held").await;
    assert!(
        held["refused"]
            .as_str()
            .expect("a rule")
            .contains(&format!("the floor for draft {draft}")),
        "a refusal inside a draft names it, like one inside a plan: {}",
        held["refused"],
    );
    send(
        &mut socket,
        serde_json::json!({"type": "answer_call", "turn": held["turn"], "step": held["step"],
                           "allow": false}),
    )
    .await;
    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(result["verdict"]["allowed"], false);
    assert!(
        !scratch.exists(),
        "naming the draft is a change to the words, not to the floor",
    );
    let _ = std::fs::remove_file(&scratch);
}

/// **Allow once**: the write the floor refused, put to a person, runs when they
/// say yes — once, and the next write in the same draft is asked again.
///
/// The dead end this closes: a draft asked to create a file, refused by the
/// floor, and nobody asked. See `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
#[tokio::test]
async fn a_write_the_floor_refuses_is_put_to_a_person_and_runs_once_on_a_yes() {
    let scratch = std::env::temp_dir().join(format!("luu-allow-once-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&scratch);
    let call = format!(
        "Writing it.\n```tool\n{{\"name\":\"write_file\",\"arguments\":         {{\"path\":{},\"content\":\"allowed once\"}}}}\n```",
        serde_json::to_string(&scratch.display().to_string()).expect("a path"),
    );
    let address = server_writable(vec![call.clone(), call, "Done.".into()], &scratch).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "write the scratch file"}),
    )
    .await;

    // The call first, then the question about it: `until` skips what it is
    // not waiting for, so a `call_held` that overtook its `tool_call` would be
    // skipped here and the wait below would time out. It did, on the page,
    // before the question went out on the turn's own channel.
    let (call, _) = until(&mut socket, "tool_call").await;
    let (held, _) = until(&mut socket, "call_held").await;
    assert_eq!(held["step"], call["step"]);

    // A stale answer is refused rather than applied to whatever is held next.
    send(
        &mut socket,
        serde_json::json!({"type": "answer_call", "turn": held["turn"], "step": 99, "allow": true}),
    )
    .await;
    let (refused, _) = until(&mut socket, "refused").await;
    assert_eq!(refused["reason"], "call");
    assert!(!scratch.exists(), "a refused answer allowed nothing");

    send(
        &mut socket,
        serde_json::json!({"type": "answer_call", "turn": held["turn"], "step": held["step"],
                           "allow": true}),
    )
    .await;
    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(result["verdict"]["allowed"], true, "{}", result["verdict"]);
    assert_eq!(result["asked"], true);
    assert_eq!(
        std::fs::read_to_string(&scratch).expect("the file a person allowed"),
        "allowed once"
    );

    // Once means once: the same write, one step later, is asked again.
    let (again, _) = until(&mut socket, "call_held").await;
    assert_eq!(again["step"], held["step"].as_u64().unwrap() + 1);
    send(
        &mut socket,
        serde_json::json!({"type": "answer_call", "turn": again["turn"], "step": again["step"],
                           "allow": false}),
    )
    .await;
    let (second, _) = until(&mut socket, "tool_result").await;
    assert_eq!(second["asked"], false);
    assert_eq!(second["verdict"]["allowed"], false);
    until(&mut socket, "ended").await;
    let _ = std::fs::remove_file(&scratch);
}

/// The other half of the floor, and the half that says it is a floor rather
/// than a wall: **a draft still reads.** Exploration is what it is for.
#[tokio::test]
async fn a_draft_turn_still_reads_what_the_policy_file_grants() {
    let address = server_with(vec![READS_SERVE_RS.into(), "That is what it says.".into()]).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "what does serve do"}),
    )
    .await;

    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(result["name"], "read_file");
    assert_eq!(
        result["verdict"]["allowed"], true,
        "the floor subtracts the write bit and nothing else: {}",
        result["verdict"]["rule"],
    );
}

/// And the gate can grant it: *add write* at the approval, and the same turn
/// goes through. The path is one nothing else reads, so the test can write it.
#[tokio::test]
async fn a_write_added_at_the_gate_goes_through() {
    let scratch = std::env::temp_dir().join(format!("luu-write-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&scratch);
    let call = format!(
        "Writing it.\n```tool\n{{\"name\":\"write_file\",\"arguments\":         {{\"path\":{},\"content\":\"written by the task\"}}}}\n```",
        serde_json::to_string(&scratch.display().to_string()).expect("a path"),
    );
    let address = server_writable(
        vec![PLAN_FOR_CARGO_TOML.into(), call, "Done.".into()],
        &scratch,
    )
    .await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "write the scratch file"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "writes": [scratch.display().to_string()],
        }),
    )
    .await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;

    let (approved, _) = until(&mut socket, "job_approved").await;
    assert_eq!(
        approved["plan"]["writes"].as_array().expect("writes").len(),
        1
    );

    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(
        result["verdict"]["allowed"], true,
        "{}",
        result["verdict"]["rule"],
    );
    assert_eq!(
        std::fs::read_to_string(&scratch).expect("the file the task wrote"),
        "written by the task",
    );
    let _ = std::fs::remove_file(&scratch);
}

/// The window filling up, over the socket: the tombstone that says what the
/// session forgot. Before it, a client watched the history bucket shrink and
/// could not tell the policy from the arithmetic.
#[tokio::test]
async fn the_window_filling_up_says_which_turns_it_dropped() {
    // Long enough that a couple of them no longer fit together.
    let answer = "padding ".repeat(60);
    let address = server_evicting(vec![PLAN.into(), answer.clone()]).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");

    assert_eq!(next_message(&mut socket).await["type"], "hello");

    // Through the gate once, so the prompts after it run inside a live job
    // rather than each buying a proposal of its own.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;
    until(&mut socket, "ended").await;

    // Then prompt until the window gives way. Bounded: a run that never cuts is
    // this test failing, not this test waiting.
    let mut evicted = None;
    for n in 0..8 {
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": format!("question {n}")}),
        )
        .await;
        loop {
            let message = next_message(&mut socket).await;
            match message["type"].as_str().expect("a typed message") {
                "evicted" => evicted = Some(message),
                "ended" => break,
                _ => continue,
            }
        }
        if evicted.is_some() {
            break;
        }
    }

    let evicted = evicted.expect("eight turns into a window this size, something left");
    let turns: Vec<u64> = evicted["turns"]
        .as_array()
        .expect("the turns that left")
        .iter()
        .map(|turn| turn.as_u64().expect("a turn number"))
        .collect();
    assert_eq!(
        turns[0], 1,
        "the oldest in the *history*, which is now turn 1: a prompt runs in the \
         draft it opens rather than buying a planning call nothing remembers",
    );
    assert!(
        turns.iter().max() < evicted["turn"].as_u64().as_ref(),
        "a turn cannot evict itself: {evicted}",
    );
    assert!(evicted["tokens"].as_u64().expect("a count") > 0);
    assert_eq!(
        evicted["counter"]["kind"], "approximate",
        "a count carries who produced it, and this one is not a measurement",
    );
    assert_eq!(evicted["policy"]["policy"], "turn");

    // The read side, folded from the same events: both halves of the mark.
    let cutting = evicted["turn"].as_u64().expect("the cutting turn");
    let api = get(&address, "/api/sessions/live/turns").await;
    let first = api
        .as_array()
        .expect("the turns")
        .iter()
        .find(|turn| turn["turn"] == 2)
        .expect("turn 2 is still in the transcript");
    assert_eq!(
        first["evicted_by"], cutting,
        "an evicted turn is kept and marked, never removed: the transcript exists \
         to show the difference between what happened and what the model still sees",
    );
    let cutter = api
        .as_array()
        .expect("the turns")
        .iter()
        .find(|turn| turn["turn"] == cutting)
        .expect("the turn that cut");
    assert_eq!(cutter["dropped"]["turns"][0], 1);
    // Turn 1 is a drafting turn now, not a planning call — it is *in* the
    // history, so it is the first thing the window gives up rather than the one
    // turn eviction could never reach.
    assert!(
        !api.as_array()
            .expect("the turns")
            .iter()
            .find(|turn| turn["turn"] == 1)
            .expect("the first drafting turn is still a turn")["evicted_by"]
            .is_null(),
        "the oldest turn in the window is the one that left it",
    );
}

/// The gate's headline number, over the socket: whether the planning call
/// produced the plan, or answered in prose and left the proposal to be the ask
/// itself. The panel used to infer it from an empty plan, which cannot tell a
/// model that ignored the format from one that declared an empty list.
#[tokio::test]
async fn a_proposal_says_whether_a_model_wrote_it_or_only_talked() {
    // The two doors, and which one each arm goes through is the point: a model
    // that emits a plan block suggests one by itself, and one that answers in
    // prose has to be asked. A 7B answering in prose is the ordinary case and
    // it must not cost the gate either way.
    for (reply, expected, asked) in [
        (PLAN, "model", false),
        ("I could add the flag in lib.rs, I think.", "prose", true),
    ] {
        let address = server_with(vec![reply.into(), ANSWER.into()]).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
            .await
            .expect("the websocket handshake");
        assert_eq!(next_message(&mut socket).await["type"], "hello");
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": "add a flag"}),
        )
        .await;

        if asked {
            until(&mut socket, "draft_opened").await;
            send(&mut socket, serde_json::json!({"type": "request_plan"})).await;
        }

        let (proposed, _) = until(&mut socket, "plan_proposed").await;
        assert_eq!(proposed["source"], expected, "{proposed}");

        // On the round, not on a job: nothing was approved, so no job exists
        // to carry it. That is the move `job_proposed` made in becoming
        // `plan_proposed`.
        let session = get(&address, "/api/sessions/live").await;
        assert_eq!(
            session["rounds"][0]["source"], expected,
            "and on the read side"
        );
        assert!(
            session["jobs"][0]["plan"].is_null(),
            "the only job is the draft it was offered inside",
        );
    }
}

/// What a person had to add before a small model's plan could run — the amend
/// rate, which is the cost of the gate. Readable only by diffing two lines of a
/// recording by hand until the view kept both.
#[tokio::test]
async fn the_view_keeps_the_plan_as_proposed_beside_the_plan_as_approved() {
    let address = server_with(vec![PLAN.into(), ANSWER.into()]).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;

    // The person adds the file the model forgot, which is the half that makes
    // narrowing survivable.
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "files": ["AGENTS.md"],
        }),
    )
    .await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;
    until(&mut socket, "job_approved").await;

    // `jobs[1]`, because `jobs[0]` is the draft the approval closed — the job
    // an approval opens comes after the one it folds.
    let job = get(&address, "/api/sessions/live").await["jobs"][1].clone();
    assert!(
        job["plan"].is_object(),
        "the second job is the plan, not another draft: {job}",
    );
    assert_eq!(
        job["proposed"]["files"],
        serde_json::json!(["Cargo.toml"]),
        "the plan as the model proposed it",
    );
    assert_eq!(
        job["plan"]["files"],
        serde_json::json!(["Cargo.toml", "AGENTS.md"]),
        "the plan as approved, which is what the sandbox is built from",
    );
}

/// The rung above the person: a job that closes itself on an exit code.
///
/// The one test that asserts the payoff rather than the field. A plan declares
/// `sh`, the person at the gate names `sh -c exit 0` as what would convince
/// them the work is finished, the turn runs it, and the job folds with nobody
/// having clicked anything. See
/// `RECORD/2026-09-02.closing-on-an-exit-code.completed.md`.
const PLAN_THAT_RUNS_SH: &str = "```plan\n{\"objective\":\"make it pass\",\
                                 \"steps\":[\"run it\"],\"files\":[],\
                                 \"commands\":[\"sh\"]}\n```";

fn runs(script: &str) -> String {
    format!(
        "Running it.\n```tool\n{{\"name\":\"run_command\",\"arguments\":\
         {{\"argv\":[\"sh\",\"-c\",\"{script}\"]}}}}\n```"
    )
}

#[tokio::test]
async fn a_green_command_closes_the_task_with_nobody_at_the_gate() {
    let address = server_running(
        vec![
            PLAN_THAT_RUNS_SH.into(),
            runs("exit 0"),
            "It passes.".into(),
        ],
        "sh",
    )
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "make the tests pass"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;

    // The one part of a plan the model was never asked for, arriving from the
    // person who is already reading the plan.
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "closes_on": "sh -c exit 0",
        }),
    )
    .await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;
    let (approved, _) = until(&mut socket, "job_approved").await;
    assert_eq!(approved["plan"]["closes_on"], "sh -c exit 0");

    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(
        result["verdict"]["allowed"], true,
        "the command has to run before its exit code can close anything: {result}",
    );
    assert_eq!(result["command"]["exit_code"], 0, "{result}");

    let (closed, _) = until(&mut socket, "job_closed").await;
    assert_eq!(
        closed["job"], 2,
        "the job, not the draft the approval folded"
    );
    assert_eq!(
        closed["by"], "exit_code",
        "which authority folded it is on the wire, or nothing can ever count the rungs",
    );
    assert!(
        closed["summary"]
            .as_str()
            .expect("a summary")
            .contains("run_command sh -c exit 0"),
        "the close still writes the evidence: {}",
        closed["summary"],
    );

    // And the read side agrees with what the socket carried, which is the one
    // property this file exists to keep proving.
    let session = get(&address, "/api/sessions/live").await;
    assert_eq!(session["jobs"][1]["state"], "closed");
    assert_eq!(
        session["jobs"][1]["closed_by"], "exit_code",
        "the job's own close; `jobs[0]` is the draft the plan was approved out of",
    );
    // **`approval` and not `user`**, which is what this line said until
    // `RECORD/2026-09-21.the-alternation-on-the-page.completed.md`: nobody
    // folded that draft — a plan was approved out of it, and the two are
    // different rungs of the same ladder `ClosedBy` exists to count.
    assert_eq!(session["jobs"][0]["closed_by"], "approval");
}

#[tokio::test]
async fn a_red_command_leaves_the_task_open() {
    let address = server_running(
        vec![
            PLAN_THAT_RUNS_SH.into(),
            runs("exit 1"),
            "It still fails.".into(),
            "Looking again.".into(),
        ],
        "sh",
    )
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "make the tests pass"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "closes_on": "sh -c exit 0",
        }),
    )
    .await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;
    until(&mut socket, "job_approved").await;
    let (result, _) = until(&mut socket, "tool_result").await;
    assert_eq!(
        result["command"]["exit_code"], 1,
        "the command has to have run for its exit code to mean anything: {result}",
    );
    until(&mut socket, "ended").await;

    // Asserted by asking for something the answer changes, rather than by
    // waiting for a message that should not arrive: inside a live job a prompt
    // is a turn, and behind a closed one it is a new proposal.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "what failed?"}),
    )
    .await;
    let (started, _) = until(&mut socket, "turn_started").await;
    assert_eq!(
        started["job"], 2,
        "a job whose condition was not met is still the live one",
    );
}

/// The gate widens a plan up to the policy file and not past it, and a closing
/// condition is no exception — one naming a command the job may not run can
/// never be met, and a job that can never close looks like one that will.
#[tokio::test]
async fn a_closing_condition_the_plan_cannot_run_is_refused() {
    let address = server_running(vec![PLAN_THAT_RUNS_SH.into(), "Done.".into()], "sh").await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "make the tests pass"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "closes_on": "cargo test",
        }),
    )
    .await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;

    let (approved, _) = until(&mut socket, "job_approved").await;
    assert!(
        approved["plan"]["closes_on"].is_null(),
        "the condition is missing from the plan that comes back, which is the feedback",
    );
}

/// What the store is for: a session that outlives the process that had it.
///
/// The assertion is not that a row exists — it is that the *second* server can
/// answer the read side's questions about a session it never ran, which is the
/// whole of "resume" that the fold alone can deliver. See
/// `RECORD/2026-09-02.sessions-in-sqlite.completed.md` for what it deliberately does
/// not deliver.
#[tokio::test]
async fn a_session_outlives_the_server_that_ran_it() {
    let dir = std::env::temp_dir().join(format!("luu-store-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let db = dir.join("sessions.db");

    let first = server_storing(vec![PLAN.into(), ANSWER.into()], &db).await;
    {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{first}/ws"))
            .await
            .expect("the websocket handshake");
        assert_eq!(next_message(&mut socket).await["type"], "hello");
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": "add a flag"}),
        )
        .await;
        until(&mut socket, "plan_proposed").await;
        send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
        // Approving no longer runs a held prompt: the work starts on the next one,
        // inside the job the approval opened.
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": "now do it"}),
        )
        .await;
        until(&mut socket, "ended").await;
    }

    // A second server, same file, nothing shared but the store.
    let second = server_storing(vec!["another answer".into()], &db).await;
    let listed = get(&second, "/api/sessions").await;
    let sessions = listed.as_array().expect("a listing");
    assert!(
        sessions.len() >= 2,
        "the live session and the one that ended: {listed}",
    );

    // The live row is the second server's, under the name every client asks
    // for; the other is the first server's session, which nothing in this
    // process is holding any more.
    let earlier = sessions
        .iter()
        .find(|row| row["id"] != "live")
        .expect("the session the first server ran");
    let id = earlier["id"].as_str().expect("its id");
    assert_ne!(
        id, "live",
        "a store keyed on `live` holds one session forever"
    );
    assert_eq!(
        earlier["turns"].as_u64(),
        Some(2),
        "the planning call is a turn too, and the fold keeps it: {earlier}",
    );

    let session = get(&second, &format!("/api/sessions/{id}")).await;
    assert_eq!(session["turns"][0]["prompt"], "add a flag");
    assert_eq!(
        session["jobs"][0]["state"], "closed",
        "`jobs[0]` is the draft, and the approval is what closed it",
    );
    assert_eq!(session["jobs"][1]["state"], "open");
    assert_eq!(
        session["turns"][1]["text"], ANSWER,
        "the fold, not a summary of it: {session}",
    );

    let turns = get(&second, &format!("/api/sessions/{id}/turns")).await;
    assert_eq!(turns.as_array().expect("its turns").len(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

/// A proposal that outlived the server that made it comes back as a gate.
///
/// Without this the job was restored in `proposed` with nothing holding it: the
/// next prompt did not belong to it, so it bought a second planning call and a
/// second proposal beside a question nobody had answered. The held prompt is
/// gone with the process that took it, so approving opens the job and starts
/// nothing — and the next prompt is a turn inside it.
#[tokio::test]
async fn a_proposal_that_outlived_its_server_comes_back_at_the_gate() {
    let dir = std::env::temp_dir().join(format!("luu-gate-resume-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let db = dir.join("sessions.db");

    // A prompt, planned, and left at the gate: nothing approves it.
    let first = server_storing(vec![PLAN.into()], &db).await;
    let session = {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{first}/ws"))
            .await
            .expect("the websocket handshake");
        let hello = next_message(&mut socket).await;
        let session = hello["session"]
            .as_str()
            .expect("the session id")
            .to_string();
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": "add a flag"}),
        )
        .await;
        until(&mut socket, "plan_proposed").await;
        session
    };

    // A second server, same file. Resuming puts the gate back up.
    let second = server_storing(vec!["a later answer".into()], &db).await;
    let resumed = post(&second, &format!("/api/sessions/{session}/resume")).await;
    assert_eq!(resumed.status(), reqwest::StatusCode::OK);

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{second}/ws"))
        .await
        .expect("the websocket handshake");
    let _ = next_message(&mut socket).await;

    // The question is back up and still answerable, with the plan the first
    // server never got an answer to.
    let resumed_view = get(&second, "/api/sessions/live").await;
    assert!(
        resumed_view["rounds"][0]["answer"].is_null(),
        "the round came back unanswered: {resumed_view}",
    );

    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    let (approved, _) = until(&mut socket, "job_approved").await;
    assert_eq!(
        approved["job"], 2,
        "the draft took 1 before the first server died, so this opens 2",
    );
    assert_eq!(approved["from"], 1, "and it closed that draft on the way");

    // Nothing runs on the approval itself — the prompt that was drafting went
    // with the process that held it — so the next prompt is the turn.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now say something"}),
    )
    .await;
    let (started, _) = until(&mut socket, "turn_started").await;
    assert_eq!(
        started["job"], 2,
        "the turn belongs to the job that was approved: {started}",
    );
    assert_eq!(started["prompt"], "now say something");

    let live = get(&second, "/api/sessions/live").await;
    assert_eq!(
        live["jobs"].as_array().expect("its jobs").len(),
        2,
        "the draft and the job it became — not a second proposal beside an \
         unanswered one: {live}",
    );
    assert_eq!(
        live["rounds"].as_array().expect("its rounds").len(),
        1,
        "and one round, answered once",
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn multi_session_lifecycle_and_switching() {
    let dir = std::env::temp_dir().join(format!("luu-test-multi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("sessions.db");

    let address = server_storing(
        vec![PLAN.into(), ANSWER.into(), PLAN.into(), ANSWER.into()],
        &db,
    )
    .await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("websocket connection");

    let hello = next_message(&mut socket).await;
    assert_eq!(hello["type"], "hello");

    // Run a turn in the initial session
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "initial prompt"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;
    until(&mut socket, "ended").await;

    // Verify initial session has 2 turns
    let live_one = get(&address, "/api/sessions/live").await;
    assert_eq!(live_one["turns"].as_array().unwrap().len(), 2);

    // Create a new session via POST /api/sessions
    let res = post(&address, "/api/sessions").await;
    assert_eq!(res.status(), reqwest::StatusCode::CREATED);
    let new_summary: Value = res.json().await.expect("json");
    let session_two_id = new_summary["id"].as_str().expect("new id").to_string();
    assert_eq!(new_summary["turns"], 0);

    // Socket received a new hello broadcast
    let (hello2, _) = until(&mut socket, "hello").await;
    assert_eq!(hello2["type"], "hello");
    let live2 = get(&address, "/api/sessions/live").await;
    assert_eq!(live2["turns"].as_array().unwrap().len(), 0);

    // Verify store has 2 sessions now: "live" (new session) and the checkpointed session one
    let listed2 = get(&address, "/api/sessions").await;
    let sessions2 = listed2.as_array().expect("listing 2");
    assert_eq!(sessions2.len(), 2);

    let session_one = sessions2
        .iter()
        .find(|s| s["id"] != "live")
        .expect("checkpointed session one");
    let session_one_id = session_one["id"].as_str().expect("id").to_string();
    assert_ne!(session_one_id, session_two_id);
    assert_eq!(session_one["turns"], 2);

    // Cannot delete active session
    let del_active = delete(&address, &format!("/api/sessions/{session_two_id}")).await;
    assert_eq!(del_active.status(), reqwest::StatusCode::BAD_REQUEST);

    // Resume session one
    let resume_res = post(&address, &format!("/api/sessions/{session_one_id}/resume")).await;
    assert_eq!(resume_res.status(), reqwest::StatusCode::OK);
    let resumed_summary: Value = resume_res.json().await.expect("json");
    assert_eq!(resumed_summary["turns"], 2);

    // Socket receives hello with session one resumed
    let (hello_resumed, _) = until(&mut socket, "hello").await;
    assert_eq!(hello_resumed["type"], "hello");
    let live_resumed = get(&address, "/api/sessions/live").await;
    assert_eq!(live_resumed["turns"].as_array().unwrap().len(), 2);

    // Now session one is active, session two is inactive. Delete session two!
    let del_res = delete(&address, &format!("/api/sessions/{session_two_id}")).await;
    assert_eq!(del_res.status(), reqwest::StatusCode::NO_CONTENT);

    // Session two is gone
    let check_del = reqwest::get(format!("http://{address}/api/sessions/{session_two_id}"))
        .await
        .unwrap();
    assert_eq!(check_del.status(), reqwest::StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_client_that_speaks_another_protocol_is_refused_out_loud() {
    let address = server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    let hello = next_message(&mut socket).await;
    assert_eq!(hello["type"], "hello");

    send(
        &mut socket,
        serde_json::json!({"type": "hello", "protocol": 4, "format": 6}),
    )
    .await;

    let refused = next_message(&mut socket).await;
    assert_eq!(refused["type"], "refused");
    assert_eq!(refused["reason"], "version");
    assert!(
        refused["detail"]
            .as_str()
            .expect("a detail")
            .contains(&format!("protocol {}", agent_core::protocol::VERSION)),
        "the refusal says what this host speaks: {refused}",
    );
    let closed = tokio::time::timeout(PATIENCE, socket.next())
        .await
        .expect("the socket stayed open");
    assert!(
        matches!(closed, None | Some(Ok(WsMessage::Close(_)))),
        "a client the host cannot parse is not left connected: {closed:?}",
    );
}

#[tokio::test]
async fn a_newer_client_is_refused_in_the_other_direction_too() {
    let address = server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    let _ = next_message(&mut socket).await;

    send(
        &mut socket,
        // One past whatever this host speaks, rather than a number that was one
        // past it when this was written: the literal became *this host's own
        // version* the day `grounded` landed, and a test asserting a refusal of
        // the version it speaks would have been asserting the opposite of its
        // own name.
        serde_json::json!({"type": "hello", "protocol": agent_core::protocol::VERSION + 1}),
    )
    .await;

    let refused = next_message(&mut socket).await;
    assert_eq!(
        refused["reason"], "version",
        "a client this host cannot parse is the same answer as one that cannot parse it",
    );
}

#[tokio::test]
async fn a_matching_client_is_greeted_and_then_ignored() {
    let address = server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    let _ = next_message(&mut socket).await;

    // The constants rather than the numbers: this test is about a *matching*
    // client, and a literal here makes every format bump look like a broken
    // handshake. See `RECORD/2026-09-08.what-a-fold-writes-down.completed.md`,
    // which bumped one and found this.
    send(
        &mut socket,
        serde_json::json!({
            "type": "hello",
            "protocol": agent_core::protocol::VERSION,
            "format": agent_core::record::FORMAT,
        }),
    )
    .await;
    // Nothing comes back: a handshake that matches is not an event in the
    // session, and the next message is the one the prompt causes.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    let (started, _) = until(&mut socket, "turn_started").await;
    assert_eq!(started["turn"], 1);
}

#[tokio::test]
async fn a_guarded_port_hears_the_handshake_before_anything_else() {
    let (address, dir) = server_guarded("s3cret").await;
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(format!("ws://{address}/ws"))
        .header("Authorization", "Bearer s3cret")
        .header("Host", address.clone())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(())
        .expect("the request");
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the websocket handshake");
    let _ = next_message(&mut socket).await;

    // The token got it through the door. It still may not drive the session
    // until it says what it speaks: reachability and version are two questions.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;

    let refused = next_message(&mut socket).await;
    assert_eq!(refused["type"], "refused");
    assert_eq!(refused["reason"], "version");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_signed_approval_runs_the_held_prompt_and_says_who_approved() {
    let (address, signer) = server_signing().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    let hello = next_message(&mut socket).await;
    let session = hello["session"]
        .as_str()
        .expect("the session id")
        .to_string();

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    let (proposed, _) = until(&mut socket, "plan_proposed").await;
    assert!(proposed["job"].is_null(), "a proposal is not a job");
    // The id the approval will hand out: the draft took 1, so this opens 2.
    let job = 2;

    // Unsigned first: this server was told approvals are signed, and the gate
    // is where that is enforced rather than at the door.
    send(
        &mut socket,
        serde_json::json!({"type": "approve_plan", "job": job}),
    )
    .await;
    let (refused, _) = until(&mut socket, "refused").await;
    assert_eq!(refused["reason"], "signature");

    let files = vec!["Cargo.toml".to_string()];
    let signature = signer
        .sign(
            &Approval {
                enforcement: None,
                session: &session,
                job,
                files: &files,
                writes: &[],
                commands: &[],
                closes_on: None,
                network: None,
                egress: None,
            },
            "jgermade",
        )
        .expect("signing");

    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "job": job,
            "files": files,
            "signature": {"by": signature.by, "sig": signature.sig},
        }),
    )
    .await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;

    let (approved, _) = until(&mut socket, "job_approved").await;
    assert_eq!(approved["approved_by"]["by"], "key");
    assert_eq!(
        approved["approved_by"]["name"], "jgermade",
        "which key, so a recording can count one authority against another",
    );

    // And the read side, folded from the same events, agrees.
    let view = get(&address, "/api/sessions/live").await;
    assert_eq!(view["jobs"][1]["approved_by"]["name"], "jgermade");
}

#[tokio::test]
async fn a_grant_widened_after_the_signature_is_refused() {
    let (address, signer) = server_signing().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("the websocket handshake");
    let hello = next_message(&mut socket).await;
    let session = hello["session"]
        .as_str()
        .expect("the session id")
        .to_string();

    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "add a flag"}),
    )
    .await;
    let (proposed, _) = until(&mut socket, "plan_proposed").await;
    assert!(proposed["job"].is_null(), "a proposal is not a job");
    // The id the approval will hand out: the draft took 1, so this opens 2. A
    // signature is bound to it, which is the one thing the gate still names.
    let job = 2;

    let signed = vec!["Cargo.toml".to_string()];
    let signature = signer
        .sign(
            &Approval {
                enforcement: None,
                session: &session,
                job,
                files: &signed,
                writes: &[],
                commands: &[],
                closes_on: None,
                network: None,
                egress: None,
            },
            "jgermade",
        )
        .expect("signing");

    // What a relay between the person and the gate would do: the same
    // signature, over one more tree.
    send(
        &mut socket,
        serde_json::json!({
            "type": "approve_plan",
            "job": job,
            "files": ["Cargo.toml", "src"],
            "signature": {"by": signature.by, "sig": signature.sig},
        }),
    )
    .await;

    let (refused, _) = until(&mut socket, "refused").await;
    assert_eq!(refused["reason"], "signature");
    let view = get(&address, "/api/sessions/live").await;
    assert_eq!(
        view["jobs"].as_array().expect("the jobs").len(),
        1,
        "a refused approval opens nothing: the draft is still the only job",
    );
    assert_eq!(view["jobs"][0]["state"], "open");
    assert!(
        view["jobs"][0]["plan"].is_null(),
        "and it is still the draft, with the plan still on the table",
    );
    assert!(
        view["rounds"][0]["answer"].is_null(),
        "the round is unanswered, which is what still waiting on a person means",
    );
}

/// The read half of the configuration modal: what a browser is told this server
/// resolved.
///
/// It asserts the *shape*, not a URL — the point of the route is that the page
/// stops having to read a terminal for facts the run already decided. The write
/// half is not here: it would write a `config.toml` on the machine running the
/// tests, and the rule it has to obey is unit-tested in `provider` against a
/// scratch path instead.
#[tokio::test]
async fn the_settings_route_reports_what_the_run_resolved() {
    let address = server().await;
    let settings: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");

    assert_eq!(settings["backend"], "mock");
    assert_eq!(settings["model"], "mock");
    // The mock is nowhere, so there is nothing to say left the machine.
    assert_eq!(settings["destination"], "");
    assert_eq!(settings["remote"], false);
    assert_eq!(settings["window_from"], "unset");
    assert!(
        settings["sandbox"]
            .as_str()
            .is_some_and(|text| text.contains("read-write")),
        "the sandbox this run resolved is on the page, not only on stderr"
    );
}

/// The settings carry the floor, which is what the gate shows beside a plan.
///
/// Row 31: the panel had always shown what a plan *asks for* and never what the
/// turn in front of it may already do, so approving looked like a yes to a list
/// rather than a decision about what to add. The page never derives this — a
/// second resolution of a policy in another language is a second sandbox — so
/// the assertion is that the route hands it over, resolved.
///
/// The two halves that make it a *floor*: the tree is there, and it is there
/// **read-only**, under a policy file that grants it read-write. See
/// `RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`.
#[tokio::test]
async fn the_settings_route_carries_the_floor_a_draft_runs_on() {
    let address = server().await;
    let settings: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");

    let floor = &settings["floor"];
    assert!(
        !floor.is_null(),
        "the floor is the other term of the gate's question"
    );
    let reads: Vec<&str> = floor["reads"]
        .as_array()
        .expect("the paths it may read")
        .iter()
        .map(|path| path.as_str().expect("a path"))
        .collect();
    assert!(
        reads.contains(&"."),
        "the base is spelled the way a policy file spells it: {reads:?}",
    );
    // The sandbox line beside it says read-write on the same tree, which is
    // what makes this a subtraction rather than a second description of the
    // same grant.
    assert!(
        settings["sandbox"]
            .as_str()
            .is_some_and(|text| text.contains("read-write")),
        "the session grants writes; the floor is what takes them away",
    );
    // No `writes` key at all, deliberately: an always-empty list is a question
    // mark where the panel says a sentence.
    assert!(
        floor.get("writes").is_none(),
        "the floor grants no writes by construction and says so in words",
    );
    assert_eq!(floor["network"], false, "the mock server reaches nothing");
    assert_eq!(
        floor["asks"], true,
        "approvals need no signature here, so a refused write is put to a person",
    );
}

/// A server that fell into the mock says so, in the one field a page is
/// allowed to read it from.
///
/// The rule is the server's — see `provider::DestinationFrom`. This test exists
/// because the page opens the providers editor on it, and a browser that worked
/// it out for itself from `backend == "mock"` would be a second implementation
/// of a rule that already has one.
#[tokio::test]
async fn a_server_with_nowhere_to_send_says_so() {
    let address = server().await;
    let settings: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");

    // `server()` builds an `App` from `Resolved::mock()`, which is the
    // resolution of a run that read no file and was given no flag.
    assert_eq!(settings["unconfigured"], true);
}

/// A session may name a provider, and a name the file does not have is refused
/// rather than quietly ignored.
///
/// The happy path is not here for the reason the write route's is not: it needs
/// a `config.toml` on the machine running the tests. What this pins is that the
/// route reads the file at all, and that a refusal leaves the session alone.
/// A posture is a name in `config.toml`, and one the file does not carry is
/// refused *before* anything is reset — the same rule a provider gets, and for
/// the same reason: the session that is running should not end because the next
/// one could not start. See
/// `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
#[tokio::test]
async fn a_session_may_not_name_a_posture_that_is_not_in_the_file() {
    let address = server().await;
    let client = reqwest::Client::new();

    let refused = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "posture": "no-such-posture" }))
        .send()
        .await
        .expect("asking for a session on a posture that does not exist");
    assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
    let said = refused.text().await.expect("a reason");
    assert!(said.contains("no-such-posture"), "{said}");

    // And what the session may do is untouched: the refusal happens before
    // anything is reset.
    let settings: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");
    assert_eq!(settings["posture"]["name"], serde_json::Value::Null);
}

/// The whole of it, end to end: a name, the policy file it points at, and a
/// session that runs under it and says which.
#[tokio::test]
async fn a_session_started_on_a_posture_runs_under_it_and_says_which() {
    let dir = std::env::temp_dir().join(format!("luu-posture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory for the policy file");
    let policy = dir.join("wide.toml");
    // Wider than the server's own in the two ways a reader compares postures
    // on, so "which one is in force" is answerable from the outside.
    std::fs::write(
        &policy,
        "[sandbox]\nnetwork = true\ncommands = [\"ls\"]\n\n         [[sandbox.paths]]\npath = \".\"\naccess = \"read-write\"\n",
    )
    .expect("a policy file");
    let mut postures = std::collections::BTreeMap::new();
    postures.insert(
        "wide".to_string(),
        luu::provider::Posture {
            policy: policy.clone(),
        },
    );

    let address = server_with_postures(
        vec![PLAN.into(), ANSWER.into()],
        Duration::ZERO,
        SandboxPolicy::default(),
        Budget::new(0, 0, Eviction::Turn),
        None,
        Approvers::default(),
        postures,
        None,
        None,
    )
    .await;
    let client = reqwest::Client::new();

    let before: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");
    assert_eq!(before["posture"]["network"], false, "the server's own");
    // The key is there and it is null. A reader that cannot tell an absent key
    // from an absent posture is one that will get it wrong, and the check in
    // `scripts/container-check.sh` is exactly that reader.
    assert!(
        before["posture"]
            .as_object()
            .expect("a posture")
            .contains_key("name"),
        "the name is always written: {before}",
    );
    assert_eq!(before["posture"]["name"], serde_json::Value::Null);

    // The page asks what may be chosen first. Read-only, because a page that
    // could write a posture could widen its own sandbox.
    let offered: serde_json::Value = reqwest::get(format!("http://{address}/api/postures"))
        .await
        .expect("asking what may be chosen")
        .json()
        .await
        .expect("the postures are JSON");
    assert!(offered["choosable"].as_bool().expect("a surface answers"));
    assert!(offered["postures"]["wide"].is_object(), "{offered}");

    let created = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "posture": "wide" }))
        .send()
        .await
        .expect("starting a session on it");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let after: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");
    assert_eq!(after["posture"]["name"], "wide");
    assert_eq!(
        after["posture"]["network"], true,
        "the policy file the posture named is the one in force: {after}",
    );
    assert!(
        after["sandbox"]
            .as_str()
            .expect("the resolved sandbox")
            .contains("ls"),
        "the commands are the posture's: {after}",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A posture is chosen when a session starts and a resume may not move it.
///
/// Which is what `luu-design.md` has said since 2026-09-08, and what the tree
/// did not do: the endpoint refused a *named* posture and then inherited a
/// different one silently, so a proposal left open under a container came back
/// at the gate on whatever the server happened to be running. See
/// `RECORD/2026-09-17.what-an-approval-was-granted-under.completed.md`.
#[tokio::test]
async fn a_resume_keeps_the_posture_a_sessions_jobs_were_approved_under() {
    let dir = std::env::temp_dir().join(format!("luu-resume-posture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let policy = dir.join("wide.toml");
    std::fs::write(&policy, "[sandbox]\nnetwork = true\ncommands = [\"ls\"]\n")
        .expect("writing the posture's policy file");

    let mut postures = std::collections::BTreeMap::new();
    postures.insert(
        "wide".to_string(),
        luu::provider::Posture {
            policy: policy.clone(),
        },
    );

    let address = server_with_postures(
        vec![PLAN.into(), ANSWER.into()],
        Duration::ZERO,
        SandboxPolicy::default(),
        Budget::new(0, 0, Eviction::Turn),
        None,
        Approvers::default(),
        postures,
        Some(dir.join("sessions.db")),
        None,
    )
    .await;
    let client = reqwest::Client::new();

    // One session on `wide`, and a second on nothing — which checkpoints the
    // first into the store and leaves the server on its own policy file.
    let created: serde_json::Value = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "posture": "wide" }))
        .send()
        .await
        .expect("starting a session on it")
        .json()
        .await
        .expect("the summary is JSON");
    let contained = created["id"].as_str().expect("an id").to_string();

    let created = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("starting a second session");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let settings: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");
    assert_eq!(
        settings["posture"]["network"], false,
        "the server is back on its own policy file: {settings}",
    );

    // The one this row exists for. Before this it answered 200 and handed the
    // session's gate to a sandbox its jobs were never approved against.
    let refused = client
        .post(format!("http://{address}/api/sessions/{contained}/resume"))
        .send()
        .await
        .expect("resuming a session stored under another posture");
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    let said = refused.text().await.expect("a reason");
    assert!(
        said.contains("wide") && said.contains("approved"),
        "the refusal names the posture and why: {said}",
    );

    // And it is not a dead end: naming the posture the session ran under
    // resumes it, because that is the one its jobs were approved against.
    let resumed = client
        .post(format!("http://{address}/api/sessions/{contained}/resume"))
        .json(&serde_json::json!({ "posture": "wide" }))
        .send()
        .await
        .expect("resuming it where it belongs");
    assert_eq!(resumed.status(), reqwest::StatusCode::OK);

    let settings: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");
    assert_eq!(settings["posture"]["name"], "wide");
    assert_eq!(
        settings["posture"]["network"], true,
        "the resumed session runs under the posture it was approved against: {settings}",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_session_may_not_name_a_provider_that_is_not_in_the_file() {
    let address = server().await;
    let client = reqwest::Client::new();

    let refused = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "provider": "no-such-profile-in-any-file" }))
        .send()
        .await
        .expect("asking for a session on a profile that does not exist");
    assert_eq!(refused.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

    // And the destination is untouched: the refusal happens before anything is
    // reset, so the session that was running is still the one that is running.
    let settings: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");
    assert_eq!(settings["backend"], "mock");

    // A body-less POST still means "the destination this server is pointed at",
    // which is what every client written before this parameter existed sends.
    let created = client
        .post(format!("http://{address}/api/sessions"))
        .send()
        .await
        .expect("asking for a session with no body at all");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
}

/// Resuming somewhere else is refused the same way starting somewhere else is,
/// and refuses *before* it touches the session that is running.
#[tokio::test]
async fn resuming_on_a_provider_that_is_not_in_the_file_changes_nothing() {
    let dir = std::env::temp_dir().join(format!("luu-resume-refusal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let db = dir.join("sessions.db");
    let address = server_storing(vec![PLAN.into(), ANSWER.into()], &db).await;
    let client = reqwest::Client::new();

    // The live session's own id, which is what a page continuing the session it
    // is watching sends.
    let live: serde_json::Value = reqwest::get(format!("http://{address}/api/sessions"))
        .await
        .expect("listing the sessions")
        .json()
        .await
        .expect("the listing is JSON");
    let id = live[0]["id"]
        .as_str()
        .expect("the live session")
        .to_string();

    let refused = client
        .post(format!("http://{address}/api/sessions/{id}/resume"))
        .json(&serde_json::json!({ "provider": "no-such-profile-in-any-file" }))
        .send()
        .await
        .expect("asking to resume on a profile that does not exist");
    assert_eq!(refused.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

    let settings: serde_json::Value = reqwest::get(format!("http://{address}/api/settings"))
        .await
        .expect("asking for the settings")
        .json()
        .await
        .expect("the settings are JSON");
    assert_eq!(
        settings["backend"], "mock",
        "the refusal happens before the checkpoint, so nothing moved",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The providers file as the editor reads it, on a loopback server.
#[tokio::test]
async fn the_providers_route_says_whether_this_surface_may_write() {
    let address = server().await;
    let response = reqwest::get(format!("http://{address}/api/providers"))
        .await
        .expect("asking for the providers");
    // A machine with a `config.toml` that does not load answers 422 with the
    // message, which is a real answer and not a failure of this route.
    if response.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
        return;
    }
    let view: serde_json::Value = response.json().await.expect("the view is JSON");
    assert_eq!(
        view["editable"], true,
        "bound on loopback, so this surface may write the file"
    );
    assert!(view["refused"].is_null());
}

/// A session can be named, and the name survives the fold being rewritten.
///
/// The one thing the page may change about a stored session other than deleting
/// it. Two halves are worth asserting and they are not the same half: the live
/// session is renamed in memory and checkpointed, a stored one is patched in
/// SQLite — and a rename that went through `save` would rebuild the row from a
/// view and silently drop the `provider` column the session picker starts from.
/// See `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
#[tokio::test]
async fn a_session_can_be_named_and_keeps_the_name() {
    let dir = std::env::temp_dir().join(format!("luu-test-rename-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("sessions.db");

    let address = server_storing(vec![PLAN.into(), ANSWER.into()], &db).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .expect("websocket connection");
    assert_eq!(next_message(&mut socket).await["type"], "hello");

    // A turn, so the live session has something to be a fold of.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "name me"}),
    )
    .await;
    until(&mut socket, "plan_proposed").await;
    send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
    // Approving no longer runs a held prompt: the work starts on the next one,
    // inside the job the approval opened.
    send(
        &mut socket,
        serde_json::json!({"type": "prompt", "text": "now do it"}),
    )
    .await;
    until(&mut socket, "ended").await;

    // The live one: renamed in memory, then checkpointed, so the listing the
    // page reads says so.
    let answer = patch(
        &address,
        "/api/sessions/live",
        serde_json::json!({"title": "the one about naming"}),
    )
    .await;
    assert_eq!(answer.status(), reqwest::StatusCode::OK);
    let listed = get(&address, "/api/sessions").await;
    let live = listed
        .as_array()
        .expect("a listing")
        .iter()
        .find(|row| row["turns"].as_u64() == Some(2))
        .expect("the session that ran a turn");
    assert_eq!(live["title"], "the one about naming");

    // Empty is refused rather than stored: a session with no name is a row
    // nobody can pick out of the history.
    let refused = patch(
        &address,
        "/api/sessions/live",
        serde_json::json!({"title": "   "}),
    )
    .await;
    assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);

    // A second session, so the first one is stored rather than live. Its id is
    // only settled at this moment — until another session starts, the live one
    // is listed under the word `live` — so the id is read after the switch and
    // not before it.
    assert_eq!(
        post(&address, "/api/sessions").await.status(),
        reqwest::StatusCode::CREATED
    );
    until(&mut socket, "hello").await;

    let listed = get(&address, "/api/sessions").await;
    let stored = listed
        .as_array()
        .expect("a listing")
        .iter()
        .find(|row| row["turns"].as_u64() == Some(2))
        .expect("the session that ran a turn");
    // The name it was given as the live session came with it.
    assert_eq!(stored["title"], "the one about naming");
    let stored_id = stored["id"].as_str().expect("an id").to_string();
    assert_ne!(stored_id, "live");

    let answer = patch(
        &address,
        &format!("/api/sessions/{stored_id}"),
        serde_json::json!({"title": "renamed while stored"}),
    )
    .await;
    assert_eq!(answer.status(), reqwest::StatusCode::OK);

    // Both places the title lives moved together: the column the listing reads
    // and the blob the fold is loaded from. A rename that moved one would be a
    // session whose name depends on which query found it.
    let listed = get(&address, "/api/sessions").await;
    let row = listed
        .as_array()
        .expect("a listing")
        .iter()
        .find(|row| row["id"] == stored_id.as_str())
        .expect("the stored session");
    assert_eq!(row["title"], "renamed while stored");
    let view = get(&address, &format!("/api/sessions/{stored_id}")).await;
    assert_eq!(view["title"], "renamed while stored");
    assert_eq!(view["turns"].as_array().unwrap().len(), 2);

    // And one that is not there is a 404 rather than a silent success.
    let missing = patch(
        &address,
        "/api/sessions/no-such-session",
        serde_json::json!({"title": "nothing"}),
    )
    .await;
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(&dir).ok();
}

/// The rules a window is rendered under are the **session's**, not the
/// process's.
///
/// Until this they arrived once, at `serve`, as parameters beside
/// `context_limit` and `evict`, and every session the process ran shared them —
/// so the only way to change one was to restart the server with a different
/// flag. That is what made them unreachable from the page, and the reason the
/// record calls this part the real work rather than the modal.
///
/// The test is two sessions in one process under different rules, and it reads
/// both surfaces that have to agree about which: `/api/settings`, which the page
/// renders, and the live fold, which is what a resume compares before it writes
/// a header. See `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`
/// part 2.
#[tokio::test]
async fn two_sessions_in_one_process_render_their_windows_under_different_rules() {
    let address = server().await;
    let client = reqwest::Client::new();

    let settings = |address: String| async move {
        reqwest::get(format!("http://{address}/api/settings"))
            .await
            .expect("asking for the settings")
            .json::<serde_json::Value>()
            .await
            .expect("the settings are JSON")
    };

    // What the process was started under, with no flags and no `[resend]`
    // table: rule A on and the other two off, which is the default as it has
    // stood since 2026-09-19. This is the end of the chain the flip travels —
    // `Budget::new` to a session's settings to the wire — and the assertion is
    // here rather than only in a unit test because every link between the two
    // is a place it could have been dropped.
    let before = settings(address.clone()).await;
    assert_eq!(
        (&before["repeat"], &before["prune"], &before["results"]),
        (
            &serde_json::json!("once"),
            &serde_json::json!("never"),
            &serde_json::json!("kept")
        ),
        "the flags this server was started with: {before}",
    );

    // A session that names one rule and nothing else. No provider, no model and
    // no posture — naming a rule must not need a profile to go with it.
    let created = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "repeat": "once" }))
        .send()
        .await
        .expect("starting a session under rule A");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let after = settings(address.clone()).await;
    assert_eq!(after["repeat"], "once", "{after}");
    assert_eq!(
        (&after["prune"], &after["results"]),
        (&serde_json::json!("never"), &serde_json::json!("kept")),
        "a rule named alone moves only itself: {after}",
    );
    assert_eq!(
        after["model"], before["model"],
        "and it moves nothing about where the session sends",
    );

    // The fold says the same thing, which is what makes it comparable on a
    // resume: a session whose rules nothing can read back is one whose
    // recording a resume cannot check itself against.
    let live = get(&address, "/api/sessions/live").await;
    assert_eq!(
        (&live["repeat"], &live["prune"], &live["results"]),
        (
            &serde_json::json!("once"),
            &serde_json::json!("never"),
            &serde_json::json!("kept")
        ),
        "the live fold: {live}",
    );

    // A second session, in the same process, under all three. This is the whole
    // claim of part 2 — it was impossible one commit ago without restarting the
    // binary.
    let created = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({
            "repeat": "always",
            "prune": "behind",
            "results": "cited",
        }))
        .send()
        .await
        .expect("starting a second session under B and C");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let third = settings(address.clone()).await;
    assert_eq!(
        (&third["repeat"], &third["prune"], &third["results"]),
        (
            &serde_json::json!("always"),
            &serde_json::json!("behind"),
            &serde_json::json!("cited")
        ),
        "the second session is not the first one's: {third}",
    );

    // And a session that names none keeps what is in place rather than
    // resetting to the flags. That is the destination's rule and not the
    // posture's, deliberately: a posture is re-resolved every session because
    // nothing may inherit a container, and these grant nothing — which is the
    // same asymmetry that withdrew the refusal on a resume.
    let created = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("starting a session that names nothing");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let fourth = settings(address.clone()).await;
    assert_eq!(
        (&fourth["repeat"], &fourth["prune"], &fourth["results"]),
        (&third["repeat"], &third["prune"], &third["results"]),
        "naming no rule is not naming the default: {fourth}",
    );

    // A value the enum does not have is refused, for the reason a provider is a
    // name out of a file and never a URL: what may be asked for is bounded by
    // what this machine has.
    let refused = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "prune": "aggressively" }))
        .send()
        .await
        .expect("asking for a rule that does not exist");
    assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
}

/// `GET /api/resend` answers two different questions and keeps them apart.
///
/// The file is *this machine's default* and outlives every session; `running`
/// is what the live session is actually rendering under. They can disagree —
/// a session started under a rule nobody wrote down is the ordinary case — and
/// a page that showed one of them as though it were the other would be telling
/// somebody their machine is set to something it is not. It is the relationship
/// `ProvidersView::running` has with the file's `default`, one table along. See
/// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md` part 3.
#[tokio::test]
async fn the_resend_route_keeps_this_machines_default_apart_from_what_is_running() {
    let address = server().await;
    let client = reqwest::Client::new();

    let resend = get(&address, "/api/resend").await;
    assert_eq!(
        (
            &resend["running"]["repeat"],
            &resend["running"]["prune"],
            &resend["running"]["results"]
        ),
        (
            &serde_json::json!("once"),
            &serde_json::json!("never"),
            &serde_json::json!("kept")
        ),
        "what this server is under: {resend}",
    );
    assert!(
        resend["editable"].as_bool().expect("a surface answers"),
        "a test server is bound on loopback, so this one may write: {resend}",
    );
    assert_eq!(
        resend["refused"],
        serde_json::Value::Null,
        "and there is nothing to refuse: {resend}",
    );

    // A session started under a rule moves `running` and leaves the file alone.
    // Nothing was written down, so the machine's default is what it was.
    let created = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "repeat": "once" }))
        .send()
        .await
        .expect("starting a session under rule A");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let after = get(&address, "/api/resend").await;
    assert_eq!(after["running"]["repeat"], "once", "{after}");
    assert_eq!(
        after["file"]["repeat"],
        serde_json::Value::Null,
        "a session's choice is not a machine's default: {after}",
    );
}

/// A session resumed **inside an approved job** comes back holding the plan it
/// was approved with.
///
/// Found while giving the draft a floor, and it predates it: the resume route
/// wrote `session.narrowed = None`, the live job stayed open across the resume,
/// so the turn's fallback arm answered and the session came back able to touch
/// everything the policy file grants. A **widening**, silent, and nothing
/// caught it — the floor is what turned the same line into a loud narrowing and
/// is the only reason it was found. Item 9's shape one field along.
///
/// The plan grants a read of `Cargo.toml` and nothing else. The turn after the
/// resume reads `src/serve.rs`, which the policy file grants and the plan does
/// not. Reading rather than writing on purpose: this test's failure mode is a
/// call that *goes through*, and the one that goes through must not be able to
/// edit the checkout.
#[tokio::test]
async fn a_resumed_session_is_still_held_by_the_plan_its_job_was_approved_with() {
    let dir = std::env::temp_dir().join(format!("luu-resume-narrowed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let db = dir.join("sessions.db");

    // One session, one approval, and nothing after it: the job is open and the
    // plan is what holds it when the process goes away.
    let first = server_storing(vec![PLAN_FOR_CARGO_TOML.into()], &db).await;
    {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{first}/ws"))
            .await
            .expect("the websocket handshake");
        assert_eq!(next_message(&mut socket).await["type"], "hello");
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": "read the manifest"}),
        )
        .await;
        until(&mut socket, "plan_proposed").await;
        send(&mut socket, serde_json::json!({"type": "approve_plan"})).await;
        until(&mut socket, "job_approved").await;
    }

    // A second server, same store, nothing else shared.
    let second = server_storing(
        vec![
            READS_SERVE_RS.into(),
            "I was not approved to read that.".into(),
        ],
        &db,
    )
    .await;
    let listed = get(&second, "/api/sessions").await;
    let id = listed
        .as_array()
        .expect("a listing")
        .iter()
        .find(|row| row["id"] != "live")
        .expect("the session the first server ran")["id"]
        .as_str()
        .expect("its id")
        .to_string();
    let resumed = post(&second, &format!("/api/sessions/{id}/resume")).await;
    assert_eq!(resumed.status(), reqwest::StatusCode::OK, "resuming {id}");

    let live = get(&second, "/api/sessions/live").await;
    assert_eq!(
        live["jobs"][1]["state"], "open",
        "the approved job has to still be open, or this test proves nothing",
    );

    {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{second}/ws"))
            .await
            .expect("the websocket handshake");
        assert_eq!(next_message(&mut socket).await["type"], "hello");
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": "now read something else"}),
        )
        .await;

        let (result, _) = until(&mut socket, "tool_result").await;
        assert_eq!(result["name"], "read_file");
        assert_eq!(
            result["verdict"]["allowed"], false,
            "the resume handed the job back the policy file: {}",
            result["verdict"]["rule"],
        );
        assert!(
            result["verdict"]["rule"]
                .as_str()
                .expect("a rule")
                .contains("the approved plan for job 2"),
            "and it has to be the plan that refused, not the floor: {}",
            result["verdict"]["rule"],
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// A resumed session's turns get their code back, and get it on the turn that
/// had it.
///
/// The surface item 20 is about, driven rather than argued. A first server
/// answers one prompt with relevance selection on, so its turns are grounded
/// with spans nobody typed; a second server over the same store resumes the
/// session and asks another.
///
/// **The assertion is a position, not a substring.** Under rule A a span is
/// rendered by the *oldest* turn of the window that carries it, so finding
/// `// <spec>` before the first prompt means an early turn is carrying it —
/// which only re-reading it at the resume can do. A session that lost its
/// fragments would either not have the span at all, or have it in the message
/// of the turn that selected it *now*, after that prompt. See
/// `RECORD/2026-09-19.fragments-by-reference.completed.md`.
#[tokio::test]
async fn a_resumed_session_renders_its_spans_on_the_turn_that_was_grounded_with_them() {
    let dir = std::env::temp_dir().join(format!("luu-resume-spans-{}", std::process::id()));
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).expect("a scratch directory");
    // Small and on purpose: see `server_storing_selecting`. One of these three
    // is what the selector will choose, and which one is its decision.
    std::fs::write(
        tree.join("window.rs"),
        "/// The window a turn is budgeted against.\npub fn window() -> u32 {\n    8192\n}\n",
    )
    .expect("a source file");
    std::fs::write(
        tree.join("clock.rs"),
        "/// What a seam waits on.\npub fn clock() -> u64 {\n    0\n}\n",
    )
    .expect("a source file");
    std::fs::write(
        tree.join("greeting.rs"),
        "/// Something the prompt does not ask about.\npub fn greet() -> &'static str {\n    \"hola\"\n}\n",
    )
    .expect("a source file");
    let db = dir.join("sessions.db");
    let asked = "where does a turn get its window from?";
    // Plain text, because a plan block would put the gate in the way of a test
    // that is not about the gate. One reply per prompt now: a prompt is a turn
    // rather than a planning call and a turn behind it.
    let plain = || vec!["an answer".to_string(), "an answer".to_string()];

    let first = server_storing_selecting(plain(), &db, 1024, &tree).await;
    {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{first}/ws"))
            .await
            .expect("the websocket handshake");
        assert_eq!(next_message(&mut socket).await["type"], "hello");
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": asked}),
        )
        .await;
        // The prompt runs in the draft it opens and the gate never appears —
        // the mock answers in prose, so it suggests no plan — which suits a
        // test that is not about the gate.
        until(&mut socket, "ended").await;
    }

    let live = get(&first, "/api/sessions/live").await;
    let grounded = live["turns"]
        .as_array()
        .expect("the turns")
        .iter()
        .find_map(|turn| {
            turn["code"]
                .as_array()
                .and_then(|code| code.first().cloned())
        })
        .expect("a turn was grounded with something — otherwise this proves nothing");
    let spec = grounded["spec"].as_str().expect("its spec").to_string();
    assert_eq!(
        grounded["origin"], "selected",
        "this surface has no --fragment, so nothing here was attached by hand",
    );

    // A second server, same file, nothing shared but the store — which is what
    // makes this a resume rather than a session that never went away.
    let second = server_storing_selecting(plain(), &db, 1024, &tree).await;
    let listed = get(&second, "/api/sessions").await;
    let id = listed
        .as_array()
        .expect("a listing")
        .iter()
        .find(|row| row["id"] != "live")
        .expect("the session the first server ran")["id"]
        .as_str()
        .expect("its id")
        .to_string();

    let resumed = post(&second, &format!("/api/sessions/{id}/resume")).await;
    assert_eq!(resumed.status(), reqwest::StatusCode::OK, "resuming {id}");

    {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{second}/ws"))
            .await
            .expect("the websocket handshake");
        assert_eq!(next_message(&mut socket).await["type"], "hello");
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": "and now?"}),
        )
        .await;
        // No gate this time, and that is not an oversight: the job approved
        // before the resume is still open, so this turn is *inside* it and
        // nothing is proposed. It is the same reason the resumed session has a
        // history at all.
        until(&mut socket, "ended").await;
    }

    let after = get(&second, "/api/sessions/live").await;
    let last = after["turns"]
        .as_array()
        .expect("the turns")
        .last()
        .expect("a turn after the resume")["turn"]
        .as_u64()
        .expect("its number");
    let prompt = get(&second, &format!("/api/sessions/live/turns/{last}/prompt")).await;
    let text = prompt["text"].as_str().expect("the prompt as sent");

    let span_at = text
        .find(&format!("// {spec}"))
        .unwrap_or_else(|| panic!("the resumed window carries no {spec}:\n{text}"));
    let asked_at = text
        .find(asked)
        .expect("the first prompt is still in the window");
    assert!(
        span_at < asked_at,
        "the span is rendered by the turn that was grounded with it, not by the one \
         that selected it again: {spec}\n{text}",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The recording once it holds what the caller is about to assert over.
///
/// **The recorder writes on its own task** — `Recorder::create` spawns it, one
/// `write_all` and one flush per line — so a line is not in the file the moment
/// the request or the message that produced it returns. Every other
/// `read_record` in the suite reads a file a `luu` process has already exited
/// from; this is the only test that reads one out from under a live server, and
/// it had been assuming the writer had run.
///
/// Waiting is not a wall-clock claim about how fast that task is, and `ready`
/// is deliberately the caller's own shape rather than a line count: under load
/// the headers landed and the turn's lines had not, which is the same race one
/// level down. The assertions stay exact and stay at the call site; a writer
/// that never runs fails here after the deadline rather than passing early. A
/// parse error counts as *not yet* for the same reason — a line half-written is
/// a line not written — and is reported if it survives the deadline.
async fn recording_once(
    path: &std::path::Path,
    what: &str,
    ready: impl Fn(&[agent_core::record::RecordLine]) -> bool,
) -> Vec<agent_core::record::RecordLine> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match luu::export::read_record(path) {
            Ok(lines) if ready(&lines) => return lines,
            Ok(lines) => assert!(
                std::time::Instant::now() < deadline,
                "ten seconds and the recording still does not hold {what}: {} lines",
                lines.len(),
            ),
            Err(error) => assert!(
                std::time::Instant::now() < deadline,
                "ten seconds and the recording never parsed: {error}",
            ),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// How many headers a recording holds, and where the last one starts.
fn headers_of(lines: &[agent_core::record::RecordLine]) -> Vec<&agent_core::record::RecordLine> {
    lines
        .iter()
        .filter(|line| matches!(line, agent_core::record::RecordLine::Header { .. }))
        .collect()
}

/// A `--record` file says which session each stretch of it belongs to.
///
/// Item 23, driven: until this, one header at process start named the **first**
/// session's backend, model, window, counter, eviction, posture and three
/// resend rules for every session after it — so a run where somebody pressed
/// *New* onto a different posture produced a file whose first line is a claim
/// about turns it does not describe.
///
/// Two assertions and they are the two halves of the decision. The **metadata**:
/// the second header names the posture the second session was started on, and
/// the first still names the one the first ran under. The **base**: a header
/// declares what the lines under it are counted from, so the second one's
/// `started_at` is the second session's own, and the lines after it are
/// relative to *that* — which is why the run waits before switching. Before the
/// re-base those lines carried the offset from process start, and the wait is
/// what separates the two bases by more than nothing.
///
/// **What a line is checked against is measured, not assumed.** The wait used to
/// be the bound as well as the separation, which made the test assert that a
/// handshake, a prompt and a mock reply all fit in 400 ms of wall clock — true
/// on an idle machine and a claim about nothing this test is for. It is now
/// checked against how long the session had been alive, on the recorder's own
/// clock, which load moves in the same direction as the lines themselves. See
/// `RECORD/2026-09-19.a-header-per-session.completed.md` and
/// `RECORD/2026-09-21.a-bound-that-is-not-a-clock.completed.md`.
#[tokio::test]
async fn a_recording_that_spans_sessions_carries_a_header_for_each() {
    use agent_core::record::RecordLine;

    let dir = std::env::temp_dir().join(format!("luu-record-headers-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let policy = dir.join("wide.toml");
    std::fs::write(&policy, "[sandbox]\nnetwork = true\ncommands = [\"ls\"]\n")
        .expect("writing the posture's policy file");
    let record = dir.join("run.jsonl");

    let mut postures = std::collections::BTreeMap::new();
    postures.insert(
        "wide".to_string(),
        luu::provider::Posture {
            policy: policy.clone(),
        },
    );

    let address = server_with_postures(
        vec!["an answer".into(), "an answer".into()],
        Duration::ZERO,
        SandboxPolicy::default(),
        Budget::new(0, 0, Eviction::Turn),
        None,
        Approvers::default(),
        postures,
        Some(dir.join("sessions.db")),
        Some(record.clone()),
    )
    .await;

    // Long enough that a line counted from the process's start could not be
    // mistaken for one counted from the second session's. It is a separation
    // between two bases and nothing else — what a line is checked against is
    // measured below, on the recorder's own clock, so this number is never a
    // claim about how fast a turn runs. See
    // `RECORD/2026-09-21.a-bound-that-is-not-a-clock.completed.md`.
    const APART: u64 = 400;
    tokio::time::sleep(Duration::from_millis(APART)).await;

    let client = reqwest::Client::new();
    let created: serde_json::Value = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "posture": "wide" }))
        .send()
        .await
        .expect("starting a session on the other posture")
        .json()
        .await
        .expect("the summary is JSON");
    let started_at = created["started_at"].as_u64().expect("its start");

    {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
            .await
            .expect("the websocket handshake");
        assert_eq!(next_message(&mut socket).await["type"], "hello");
        send(
            &mut socket,
            serde_json::json!({"type": "prompt", "text": "after the switch"}),
        )
        .await;
        // The prompt runs, in the draft it opens. Nothing is left hanging at
        // the gate — the mock answers in prose, so it suggests no plan — and
        // the next thing this test does is start a third session.
        until(&mut socket, "ended").await;
        until(&mut socket, "draft_opened").await;
    }

    // How long the second session had been alive when the last line this test
    // waits for was written. Every `at_ms` is stamped when its event is handled
    // (`serve.rs`), against this same clock and this same base, so a re-based
    // line cannot exceed it and a line counted from process start is exactly
    // one `APART` over it. Read here rather than turned into a constant: a
    // loaded machine moves this and the lines it bounds together.
    let alive = luu::session::now_ms().saturating_sub(started_at);

    // Two headers *and* a line under the second: the turn is what the bound
    // below is about, and a file holding only the headers is a file the writer
    // has not caught up with.
    let lines = recording_once(
        &record,
        "two headers and a line under the second",
        |lines| {
            let headers = headers_of(lines);
            headers.len() == 2
                && lines
                    .iter()
                    .skip_while(|line| !std::ptr::eq(*line, headers[1]))
                    .any(|line| {
                        matches!(line, RecordLine::Protocol { .. } | RecordLine::Trace { .. })
                    })
        },
    )
    .await;
    let headers: Vec<&RecordLine> = lines
        .iter()
        .filter(|line| matches!(line, RecordLine::Header { .. }))
        .collect();
    assert_eq!(
        headers.len(),
        2,
        "one header per session, and this file holds two",
    );

    let network_of = |line: &RecordLine| match line {
        RecordLine::Header { posture, .. } => posture.as_ref().map(|one| one.network),
        _ => None,
    };
    assert_eq!(
        network_of(headers[0]),
        Some(false),
        "the first session ran under the server's own policy file",
    );
    assert_eq!(
        network_of(headers[1]),
        Some(true),
        "and the second under the posture it was started on",
    );

    let RecordLine::Header {
        started_at: first, ..
    } = headers[0]
    else {
        unreachable!("filtered above")
    };
    let RecordLine::Header {
        started_at: second, ..
    } = headers[1]
    else {
        unreachable!("filtered above")
    };
    assert_eq!(
        *second, started_at,
        "the base a header declares is the session's own start",
    );
    assert!(
        second.saturating_sub(*first) >= APART,
        "the two bases are far enough apart that a line counted from the wrong one \
         says so: {first} then {second}",
    );

    let after = lines
        .iter()
        .skip_while(
            |line| !matches!(line, RecordLine::Header { started_at, .. } if started_at == second),
        )
        .filter_map(|line| match line {
            RecordLine::Protocol { at_ms, .. } | RecordLine::Trace { at_ms, .. } => Some(*at_ms),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!after.is_empty(), "the second session recorded nothing");
    assert!(
        after.iter().all(|at_ms| *at_ms <= alive),
        "every line under the second header is counted from it, so none of them can \
         be older than the session: {after:?} against {alive}",
    );

    // And the other call site, where the base goes **backwards**. A third
    // session checkpoints the second, and resuming the second re-bases the file
    // to a `started_at` that has already been in it — so the lines after that
    // header carry the offset from a moment further back than the one above
    // them. That is the shape of the sentence the record makes a point of: a
    // resumed session started when it started.
    let third = client
        .post(format!("http://{address}/api/sessions"))
        .json(&serde_json::json!({ "posture": "wide" }))
        .send()
        .await
        .expect("a third session, which checkpoints the second");
    assert_eq!(third.status(), reqwest::StatusCode::CREATED);

    let id = created["id"].as_str().expect("the second session's id");
    let resumed = client
        .post(format!("http://{address}/api/sessions/{id}/resume"))
        // Named here and not inherited: a resume resolves its own posture, and
        // one that named nothing would be resuming onto the policy file — which
        // is the refusal item 9 landed.
        .json(&serde_json::json!({ "posture": "wide" }))
        .send()
        .await
        .expect("resuming it");
    let status = resumed.status();
    let body = resumed.text().await.unwrap_or_default();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");

    let lines = recording_once(&record, "four headers", |lines| {
        headers_of(lines).len() == 4
    })
    .await;
    let bases: Vec<u64> = lines
        .iter()
        .filter_map(|line| match line {
            RecordLine::Header { started_at, .. } => Some(*started_at),
            _ => None,
        })
        .collect();
    assert_eq!(
        bases.len(),
        4,
        "two sessions, a third, and a resume: {bases:?}"
    );
    assert_eq!(
        bases[3], started_at,
        "a resume re-bases to the session's own start, not to now: {bases:?}",
    );
    assert!(
        bases[3] < bases[2],
        "which means the base moved backwards, and that is not a bug: {bases:?}",
    );

    let _ = std::fs::remove_dir_all(&dir);
}
