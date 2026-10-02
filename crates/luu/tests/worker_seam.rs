//! The seam, driven end to end: a real `luu worker` on the other side of a real
//! pipe.
//!
//! Under `Runtime::Direct`, which isolates nothing and is here for exactly this
//! — a seam whose only test needed Docker would be a seam tested nowhere. What
//! it does exercise is everything but the container: the handshake, the
//! manifest, the policy crossing, the authority crossing, and the property the
//! whole design rests on — **that a contained run and a host run of the same
//! call answer the same bytes**.
//!
//! See `RECORD/2026-09-02.the-worker-and-the-seam.completed.md`.

use std::path::{Path, PathBuf};

use agent_core::sandbox::{Access, Authority, PathRule, Sandbox, SandboxPolicy};
use agent_core::tools::{ToolCall, Tools};
use agent_core::worker::{Executor, PROTOCOL, Runtime, Worker, WorkerSpec};

/// A throwaway project, because the base is what the worker resolves against.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("luu-worker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        Self {
            root: root.canonicalize().unwrap_or(root),
        }
    }

    fn sandbox(&self, commands: &[&str]) -> Sandbox {
        let policy = SandboxPolicy {
            paths: vec![PathRule::new(&self.root, Access::ReadWrite)],
            commands: commands.iter().map(|name| (*name).to_string()).collect(),
            ..SandboxPolicy::default()
        };
        Sandbox::new(&policy, &self.root).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn spec(base: &Path) -> WorkerSpec {
    WorkerSpec::new(Runtime::Direct, base).with_binary(Some(env!("CARGO_BIN_EXE_luu").into()))
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.to_string(),
        arguments,
    }
}

#[tokio::test]
async fn the_worker_greets_before_it_is_asked_anything() {
    let fixture = Fixture::new("hello");
    let worker = Worker::start(&spec(&fixture.root), &["ls".into(), "not-a-program".into()])
        .await
        .expect("the worker started");

    assert_eq!(worker.hello().protocol, PROTOCOL);
    assert_eq!(worker.hello().version, env!("CARGO_PKG_VERSION"));
    // The third failure mode, answered by the only process that can see the
    // image: granted by the policy, absent from it.
    assert_eq!(worker.hello().present(), ["ls"]);
    assert_eq!(worker.hello().absent(), ["not-a-program"]);

    let described = worker.describe().expect("a worker says where it is");
    assert!(described.contains("direct (no container)"), "{described}");
    assert!(
        described.contains("granted by the policy, absent from the image"),
        "{described}"
    );
}

#[tokio::test]
async fn a_contained_call_and_a_host_call_answer_the_same_bytes() {
    // The property every probe depends on. If these diverged, a run inside the
    // container and a run outside it would not be one flag apart, and no
    // measurement taken under one would say anything about the other.
    let fixture = Fixture::new("parity");
    let sandbox = fixture.sandbox(&[]);
    let tools = Tools::standard();
    let worker = Worker::start(&spec(&fixture.root), sandbox.commands())
        .await
        .expect("the worker started");

    for call in [
        call("read_file", serde_json::json!({"path": "src/main.rs"})),
        call("list_dir", serde_json::json!({"path": "."})),
        // A denial, which is the half that would be easiest to get wrong: the
        // rule text names a resolved path and an authority, and both are
        // reconstructed on the far side rather than sent.
        call("read_file", serde_json::json!({"path": "/etc/passwd"})),
        // And a tool that does not exist, which is answered by the same
        // registry on both sides.
        call("no_such_tool", serde_json::json!({})),
    ] {
        let here = Executor::call(&tools, &call, &sandbox).await;
        let there = worker.call(&call, &sandbox).await;
        assert_eq!(there.verdict, here.verdict, "{}", call.name);
        assert_eq!(there.output, here.output, "{}", call.name);
        assert_eq!(there.error, here.error, "{}", call.name);
    }
}

#[tokio::test]
async fn a_plans_narrowing_survives_the_pipe() {
    // A denial that reached the host having lost which authority refused would
    // read as a policy refusal whatever it was, and `narrowing` argues at
    // length that those are different runs.
    let fixture = Fixture::new("authority");
    let sandbox = fixture.sandbox(&[]).under(Authority::Plan(7));
    let worker = Worker::start(&spec(&fixture.root), &[])
        .await
        .expect("the worker started");

    let outcome = worker
        .call(
            &call("read_file", serde_json::json!({"path": "/etc/hostname"})),
            &sandbox,
        )
        .await;
    assert!(!outcome.verdict.allowed);
    assert!(
        outcome.verdict.rule.contains("the approved plan for job 7"),
        "{}",
        outcome.verdict.rule
    );
}

#[tokio::test]
async fn a_write_the_plan_did_not_grant_is_refused_on_the_far_side() {
    // The sandbox is not advisory across the pipe: the worker resolves the
    // policy it was sent and refuses on its own, which is what makes the
    // container one enforcement point rather than a second opinion.
    let fixture = Fixture::new("write");
    let policy = SandboxPolicy {
        paths: vec![PathRule::new(&fixture.root, Access::Read)],
        ..SandboxPolicy::default()
    };
    let sandbox = Sandbox::new(&policy, &fixture.root).unwrap();
    let worker = Worker::start(&spec(&fixture.root), &[])
        .await
        .expect("the worker started");

    let outcome = worker
        .call(
            &call(
                "write_file",
                serde_json::json!({"path": "src/main.rs", "content": "nope"}),
            ),
            &sandbox,
        )
        .await;
    assert!(!outcome.verdict.allowed, "{:?}", outcome.verdict);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("src/main.rs")).unwrap(),
        "fn main() {}\n",
        "a refused write must not have happened"
    );
}

#[tokio::test]
async fn the_images_own_trees_are_granted_on_the_far_side_and_nowhere_else() {
    // `[[worker.paths]]` exists because `[sandbox]` has to resolve on *both*
    // sides of the pipe, and `/usr/local/cargo` is the image's toolchain — not
    // a directory on the Mac that starts the container. A grant that the host
    // must not try to resolve cannot live in the shared policy.
    let fixture = Fixture::new("image-paths");
    let outside = fixture.root.parent().unwrap().join(format!(
        "luu-worker-image-paths-outside-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("toolchain"), "in the image\n").unwrap();

    let sandbox = fixture.sandbox(&[]);
    let worker = Worker::start(
        &spec(&fixture.root).with_paths(vec![PathRule::new(&outside, Access::Read)]),
        &[],
    )
    .await
    .expect("the worker started");

    let read = call(
        "read_file",
        serde_json::json!({"path": outside.join("toolchain")}),
    );
    // Granted on the far side...
    let there = worker.call(&read, &sandbox).await;
    assert!(there.verdict.allowed, "{:?}", there.verdict);
    assert_eq!(there.output.trim(), "in the image");
    // ...and never added to the session's own policy, which the host resolved
    // and which nothing here widened.
    let here = Executor::call(&Tools::standard(), &read, &sandbox).await;
    assert!(!here.verdict.allowed, "{:?}", here.verdict);

    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn a_runtime_that_is_not_installed_fails_by_name_rather_than_hanging() {
    // The failure a person meets first: `[worker] runtime = "podman"` on a
    // machine with no podman. It has to name the command, because "starting
    // the worker failed" is a bug report nobody can act on — and it has to
    // fail rather than wait on a pipe nothing will ever write to.
    let fixture = Fixture::new("missing");
    let error = Worker::start(
        &WorkerSpec::new(Runtime::Direct, &fixture.root)
            .with_binary(Some("definitely-not-a-container-runtime".into())),
        &[],
    )
    .await
    .expect_err("a runtime that is not there cannot start a worker");
    assert!(
        error
            .to_string()
            .contains("definitely-not-a-container-runtime"),
        "{error}"
    );
}

#[tokio::test]
async fn a_contained_runtime_with_no_image_says_which_line_is_missing() {
    let fixture = Fixture::new("no-image");
    let error = Worker::start(&WorkerSpec::new(Runtime::Podman, &fixture.root), &[])
        .await
        .expect_err("a container runtime needs an image");
    assert!(error.to_string().contains("needs an image"), "{error}");
}

#[tokio::test]
async fn a_plans_network_narrowing_crosses_the_seam() {
    let fixture = Fixture::new("network-seam");
    let session_policy = SandboxPolicy {
        paths: vec![PathRule::new(&fixture.root, Access::ReadWrite)],
        network: true,
        ..SandboxPolicy::default()
    };
    let session = Sandbox::new(&session_policy, &fixture.root).unwrap();
    assert!(session.network());

    let plan = agent_core::task::Plan {
        network: false,
        ..Default::default()
    };
    let narrowed = plan.narrow(&session, 1).unwrap();
    assert!(!narrowed.network());

    let wire = agent_core::worker::WireSandbox::of(&narrowed, &[]);
    assert!(!wire.policy.network, "wire policy has network disabled");
    let resolved = wire.resolve().unwrap();
    assert!(
        !resolved.network(),
        "resolved sandbox on far side has network disabled"
    );
}

/// A worker that greets, then hangs on the first call and answers every one
/// after it — the third failure mode, which no real `luu worker` can be asked
/// to perform on demand.
///
/// Under `Runtime::Direct` the "binary" is whatever the spec names, so a shell
/// script is a worker as far as the seam is concerned. That is the point of the
/// mode: the seam is exercised without a container, and here without even a
/// `luu` on the far side.
#[cfg(unix)]
fn stuck_once(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = root.join("stuck-worker.sh");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
printf '%s\n' '{{"kind":"hello","protocol":{PROTOCOL},"version":"stuck","commands":[]}}'
while IFS= read -r line; do
  if [ ! -f "{root}/wedged" ]; then
    : > "{root}/wedged"
    sleep 60
  fi
  printf '%s\n' '{{"kind":"error","message":"a second worker answered"}}'
done
"#,
            root = root.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
#[tokio::test]
async fn a_worker_that_is_alive_and_stuck_is_abandoned_and_replaced() {
    // The clock is the agent loop's, not the seam's
    // (`RECORD/2026-09-05.a-clock-where-there-is-no-seam.completed.md`): the
    // loop is the only thing above *both* places a tool can run, so it holds
    // one deadline and tells the executor what to do with what it dropped.
    // This test is that loop, in three lines, against a worker that greets and
    // then never answers.
    let fixture = Fixture::new("stuck");
    let worker = Worker::start(
        &spec(&fixture.root).with_binary(Some(stuck_once(&fixture.root))),
        &[],
    )
    .await
    .expect("the worker greeted");

    let sandbox = fixture.sandbox(&[]);
    let read = call("read_file", serde_json::json!({"path": "src/main.rs"}));

    let gave_up = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        worker.call(&read, &sandbox),
    )
    .await;
    assert!(gave_up.is_err(), "a stuck worker does not answer");
    assert_eq!(worker.restarts(), 0, "nothing has been replaced yet");

    // What dropping the future cannot do: the child is still there, holding
    // half an exchange on a pipe that must never be reused.
    worker.abandon().await;

    // A worker holds no state between calls — every request carries its own
    // WireSandbox — so the replacement is not a recovery. It is the same
    // worker, started again, and the session's remaining calls are not owed to
    // a corpse.
    let second = worker.call(&read, &sandbox).await;
    assert!(
        second
            .error
            .unwrap_or_default()
            .contains("a second worker answered"),
        "the second call reached a worker the first one did not have",
    );
    assert_eq!(
        worker.restarts(),
        1,
        "and it is counted: a silent restart reads exactly like a slow command",
    );
    assert!(
        worker
            .describe()
            .expect("a worker says where it is")
            .contains("restarts   1"),
        "a number nobody can read is one nobody can act on",
    );
}

/// A runtime that will not start the container says why on stderr and exits —
/// `colima is not running` — and the error carries those words, not only the
/// exit code.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_that_will_not_start_says_why_in_its_own_words() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("refuses");
    let runtime = fixture.root.join("runtime.sh");
    std::fs::write(
        &runtime,
        "#!/bin/sh\necho 'level=fatal msg=\"colima is not running\"' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();

    let spec = WorkerSpec::new(Runtime::Direct, &fixture.root).with_binary(Some(runtime));
    let Err(error) = Worker::start(&spec, &[]).await else {
        panic!("a worker that exits at once is not started");
    };
    let error = error.to_string();
    assert!(error.contains("exit status: 1"), "{error}");
    assert!(error.contains("colima is not running"), "{error}");
}
