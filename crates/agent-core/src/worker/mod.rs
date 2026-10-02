//! Where tools actually run, when that is not this process.
//!
//! Level 3 of the sandbox (`RECORD/2026-08-27.tools-and-sandbox.completed.md`), in
//! the shape `RECORD/2026-09-02.the-worker-and-the-seam.completed.md` settled: **all
//! tools inside the container, the context manager and the model client
//! outside**, and one enforcement point rather than two mechanisms wearing one
//! config file.
//!
//! The seam is [`Executor`], which has one method. [`crate::tools::Tools`]
//! implements it by running the call here; [`Worker`] implements it by writing
//! the call down a pipe to a `luu worker` on the other side — a plain child
//! process under [`Runtime::Direct`], and the container's only process under
//! everything else.
//!
//! What crosses the pipe is a [`WireSandbox`]: the **policy**, not the resolved
//! sandbox. A resolved sandbox is a set of canonical paths on a filesystem the
//! worker does not have, and the container's `/usr` is the image's.
//!
//! The tool *definitions* deliberately do not cross. They are the second half
//! of the cached prefix, byte-stable across processes on purpose, and a prefix
//! assembled inside the image is a prefix that moves when the image is rebuilt.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::sandbox::{Authority, PathRule, Sandbox, SandboxPolicy, Verdict};
use crate::tools::{ToolCall, ToolFuture, ToolOutcome, Tools};

pub mod runtime;

pub use runtime::{Runtime, RuntimeError, TerminalLine, WorkerConfig, WorkerSpec};

/// The IPC's version, bumped when a reader of the old one could not parse the
/// new one.
///
/// It is checked in the handshake and a mismatch is a refusal naming both
/// numbers, because the one thing in this design that is easiest to leave stale
/// is an image: the host is a binary someone just built and the worker is
/// whatever was in the image the last time it was made.
pub const PROTOCOL: u32 = 1;

/// One thing the host asks the worker to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Run a tool call under this sandbox.
    Call {
        call: ToolCall,
        sandbox: WireSandbox,
    },
}

/// What the worker says back. Exactly one per [`Request`], in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Sent once, unprompted, before anything else.
    Hello(Hello),
    Outcome(Box<ToolOutcome>),
    /// The worker could not even get as far as a verdict — a malformed line, a
    /// policy that would not resolve on its side. Distinct from a denial, which
    /// is an [`Response::Outcome`] with an unallowed verdict in it.
    Error {
        message: String,
    },
}

/// What the worker is, said before it is asked anything.
///
/// `commands` is the finding `the-container-decided` named and did not build: a
/// **third** failure mode, distinct from *denied by the policy* and *the kernel
/// will not hold it* — **granted by the policy, absent from the image**. The
/// worker is the only thing standing on the image's `PATH`, so it is the only
/// thing that can answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// The worker's own `luu` version, so a stale image is a fact rather than a
    /// mystery.
    pub version: String,
    /// Every command the policy allows, and where it resolved — `None` when it
    /// is not on this side's `PATH` at all.
    pub commands: Vec<CommandOnPath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOnPath {
    pub name: String,
    pub path: Option<PathBuf>,
}

impl Hello {
    /// The commands the policy granted that this side does not have. The
    /// difference between a policy and the image it is meant to run in.
    pub fn absent(&self) -> Vec<&str> {
        self.commands
            .iter()
            .filter(|found| found.path.is_none())
            .map(|found| found.name.as_str())
            .collect()
    }

    pub fn present(&self) -> Vec<&str> {
        self.commands
            .iter()
            .filter(|found| found.path.is_some())
            .map(|found| found.name.as_str())
            .collect()
    }
}

/// A sandbox as it crosses a process boundary: what it is built *from*.
///
/// `Plan::narrow` is the proof this is the right shape — narrowing a session's
/// sandbox for one task is already written as *build a policy, resolve it*, and
/// this is the same three lines with a pipe in the middle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSandbox {
    /// The working directory relative paths resolve against. **The same
    /// absolute path on both sides**: the base is mounted into the container
    /// where it lives on the host, so that a run inside and a run outside can
    /// be diffed rather than merely compared.
    pub base: PathBuf,
    pub policy: SandboxPolicy,
    /// Which authority granted this — the policy file, or a task's approved
    /// plan. Carried so a denial from inside the worker still says which.
    pub authority: Authority,
}

impl WireSandbox {
    /// `image_paths` are the trees that exist only on the far side — the
    /// image's toolchain. They are added here rather than to the session's
    /// policy because the host cannot resolve them: a granted path that is not
    /// there is a load error, and `/usr/local/cargo` is not there on a Mac.
    pub fn of(sandbox: &Sandbox, image_paths: &[PathRule]) -> Self {
        let mut policy = sandbox.to_policy();
        policy.paths.extend_from_slice(image_paths);
        Self {
            base: sandbox.base().to_path_buf(),
            policy,
            authority: sandbox.authority().clone(),
        }
    }

    /// The same sandbox, resolved against *this* machine — which is the point:
    /// the implicit system roots a child needs are the ones that are here.
    pub fn resolve(&self) -> Result<Sandbox, crate::sandbox::SandboxError> {
        Ok(Sandbox::new(&self.policy, &self.base)?.under(self.authority.clone()))
    }
}

/// Where a tool call is executed. The one seam level 3 needed.
///
/// One method, because everything else [`Tools`] does is the *prefix* rather
/// than execution, and the prefix stays on the host.
pub trait Executor: Send + Sync {
    fn call<'a>(&'a self, call: &'a ToolCall, sandbox: &'a Sandbox) -> ToolFuture<'a>;

    /// The loop gave up on a call and dropped it. Deal with what is left.
    ///
    /// Dropping the future is what abandons the work; this is the part dropping
    /// cannot do — a worker process is still holding the other end of a pipe
    /// with a half-finished exchange on it, and the only sound thing to do with
    /// it is end it. In this process there is nothing to end, which is why the
    /// default does nothing rather than every executor having to say so.
    fn abandon(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }

    /// What a session says about where its tools run. `None` for the in-process
    /// executor: there is nothing to say that the rest of `luu tools` does not
    /// already say.
    fn describe(&self) -> Option<String> {
        None
    }
}

impl Executor for Tools {
    fn call<'a>(&'a self, call: &'a ToolCall, sandbox: &'a Sandbox) -> ToolFuture<'a> {
        Box::pin(async move { Tools::call(self, call, sandbox).await })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("{0}")]
    Runtime(#[from] RuntimeError),
    #[error("starting the worker with `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the worker speaks protocol {theirs} and this build speaks {ours}: \
         the image is from a different version of luu, so rebuild it"
    )]
    Protocol { ours: u32, theirs: u32 },
    #[error("the worker said nothing before it was asked: {0}")]
    NoHello(String),
}

/// The host's half: a `luu worker` on the other end of a pipe.
///
/// Long-lived and one per session, which under a container runtime means the
/// container is long-lived and one per session — because the container *is* this
/// process. `--rm` plus a closed stdin is the whole of the cleanup, and there is
/// no way to leave one running after the session that owned it died.
pub struct Worker {
    /// How it was started, for the verdict line. Kept as text rather than as
    /// the spec so that a reader of a recording sees what a reader of the
    /// terminal saw.
    label: String,
    /// The image's own trees, added to every sandbox that crosses. See
    /// [`WireSandbox::of`].
    image_paths: Vec<PathRule>,
    hello: Hello,
    /// How this one was started, kept because a worker that had to be killed is
    /// replaced by starting it again. Nothing else needs it: a worker holds no
    /// state between calls, which is what makes a restart sound rather than a
    /// recovery.
    spec: WorkerSpec,
    /// What the policy allows, as the handshake asks it. Kept for the same
    /// reason as the spec.
    commands: Vec<String>,
    /// How many times this session has had to replace its worker. Counted
    /// because a silent restart is indistinguishable from a slow command, and
    /// those two have different fixes.
    restarts: std::sync::atomic::AtomicU32,
    /// One call at a time. The agent loop is sequential and this makes that a
    /// fact rather than an assumption — two interleaved calls on one pipe would
    /// pair the wrong outcome with the wrong call.
    pipe: Mutex<Option<Pipe>>,
    /// The name of the container the live pipe talks to, for a terminal to
    /// `exec` into. **Beside the pipe rather than inside it**, because the
    /// pipe's lock is held for the whole of a tool call, and a terminal should
    /// not wait behind a `cargo build`. `None` when there is no container, or
    /// while the worker is dead and waiting for the next call to restart it.
    /// See `RECORD/2026-10-01.a-terminal-in-the-container.completed.md`.
    container: std::sync::Mutex<Option<String>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The pipe is a pair of file descriptors; the label and the handshake
        // are the parts anyone reading a failure wants.
        f.debug_struct("Worker")
            .field("label", &self.label)
            .field("hello", &self.hello)
            .finish_non_exhaustive()
    }
}

struct Pipe {
    /// The container this pipe's worker is the only process of, by name.
    container: Option<String>,
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    /// The last lines the process wrote to stderr, and the task copying them
    /// there. See [`Pipe::epitaph`].
    said: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    said_done: Option<tokio::task::JoinHandle<()>>,
}

/// How many of the last stderr lines an epitaph quotes.
const EPITAPH_LINES: usize = 4;

impl Worker {
    /// Starts a worker and completes the handshake.
    ///
    /// `commands` is what the policy allows, sent so the worker can answer
    /// which of them its `PATH` actually has. It is asked once, at startup,
    /// rather than per call: an image does not change while it is running, and
    /// the whole value of the answer is having it *before* a model finds the
    /// gap.
    pub async fn start(spec: &WorkerSpec, commands: &[String]) -> Result<Self, WorkerError> {
        let label = spec.label();
        let (pipe, hello) = Self::open(spec, commands).await?;
        let container = std::sync::Mutex::new(pipe.container.clone());

        Ok(Self {
            label,
            image_paths: spec.paths.clone(),
            hello,
            spec: spec.clone(),
            commands: commands.to_vec(),
            restarts: std::sync::atomic::AtomicU32::new(0),
            pipe: Mutex::new(Some(pipe)),
            container,
        })
    }

    /// Spawns one worker and completes its handshake. The whole of what
    /// [`Worker::start`] does before it has a `Worker`, and the whole of what a
    /// restart repeats — the same three checks, so a worker started an hour into
    /// a session is held to what the one at the start was.
    async fn open(spec: &WorkerSpec, commands: &[String]) -> Result<(Pipe, Hello), WorkerError> {
        let container = spec.runtime.is_contained().then(runtime::container_name);
        let argv = spec.argv_as(commands, container.as_deref())?;

        let mut command = tokio::process::Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| WorkerError::Spawn {
            command: argv.join(" "),
            source,
        })?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));
        // Still on this process's stderr, line by line, as it was when it was
        // inherited — and the last few kept, because a runtime that refuses to
        // start (`colima is not running`) says why only there, and the exit
        // code alone is not something anyone can act on.
        let said = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let mut lines = BufReader::new(child.stderr.take().expect("stderr was piped")).lines();
        let kept = said.clone();
        let said_done = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("{line}");
                let mut kept = kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if kept.len() == EPITAPH_LINES {
                    kept.pop_front();
                }
                kept.push_back(line);
            }
        });
        let mut pipe = Pipe {
            container,
            child,
            stdin,
            stdout,
            said,
            said_done: Some(said_done),
        };

        let hello = match pipe.read().await {
            Ok(Some(Response::Hello(hello))) => hello,
            Ok(Some(Response::Error { message })) => return Err(WorkerError::NoHello(message)),
            Ok(Some(other)) => {
                return Err(WorkerError::NoHello(format!("{other:?}")));
            }
            Ok(None) => return Err(WorkerError::NoHello(pipe.epitaph().await)),
            Err(error) => return Err(WorkerError::NoHello(error)),
        };
        if hello.protocol != PROTOCOL {
            return Err(WorkerError::Protocol {
                ours: PROTOCOL,
                theirs: hello.protocol,
            });
        }
        Ok((pipe, hello))
    }

    /// One call that did not happen, said in the worker's own name.
    ///
    /// A failed call rather than a panic: the turn is allowed to see this and
    /// carry on, which is the same courtesy a denial gets.
    fn gave_up(&self, reason: impl std::fmt::Display) -> ToolOutcome {
        let said = format!("{}: {reason}", self.label);
        ToolOutcome::failed(Verdict::deny(said.clone()), said)
    }

    /// How many workers this session has had to replace.
    ///
    /// Zero on a healthy session, and the number that says *your image is
    /// broken* rather than *your command was slow* — which is the difference
    /// nobody could see while a restart was silent.
    pub fn restarts(&self) -> u32 {
        self.restarts.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Ends this worker for good: the session that owned it is over.
    ///
    /// The same kill [`Worker::replace`] makes, and a different claim — nothing
    /// is expected to start another. It is public because the container's
    /// lifetime is supposed to *be* the session's, and until a caller could say
    /// "this session is over" the only thing that ended one was the process
    /// exiting. See `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
    pub async fn end(&self) {
        self.replace().await;
    }

    /// Ends the current worker, if there is one, and leaves the next call to
    /// start another. What [`Executor::abandon`] does here.
    async fn replace(&self) {
        if let Some(mut pipe) = self.pipe.lock().await.take() {
            self.name(None);
            pipe.kill().await;
        }
    }

    fn name(&self, container: Option<String>) {
        *self
            .container
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = container;
    }

    /// The lines that open and end a shell in this worker's container, or why
    /// there is none: no container at all, or a worker that is dead and waiting
    /// for the next tool call to start it again.
    pub fn terminal_argv(&self) -> Result<TerminalLine, String> {
        if !self.spec.runtime.is_contained() {
            return Err(format!(
                "{} runs tools without a container, so there is nothing to open a terminal in",
                self.label
            ));
        }
        let name = self
            .container
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| {
                "the worker is not running; it starts again on the next tool call".to_string()
            })?;
        static OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.spec
            .exec_argv(&name, &format!("{}-{id}", std::process::id()))
            .ok_or_else(|| format!("{} cannot exec into a container", self.label))
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether there is a container around this worker. `direct` has none:
    /// its worker is a child on this machine.
    pub fn is_contained(&self) -> bool {
        self.spec.runtime.is_contained()
    }
}

impl Executor for Worker {
    fn call<'a>(&'a self, call: &'a ToolCall, sandbox: &'a Sandbox) -> ToolFuture<'a> {
        Box::pin(async move {
            let request = Request::Call {
                call: call.clone(),
                sandbox: WireSandbox::of(sandbox, &self.image_paths),
            };
            let mut slot = self.pipe.lock().await;
            // A worker that is gone — killed for being stuck, or exited on its
            // own — is replaced here rather than at the moment it died. It holds
            // no state between calls, so the new one is not a recovery: it is
            // the same worker, started again.
            if slot.is_none() {
                // The restart is inside the call, so it is inside the loop's
                // deadline: a runtime that hangs while creating a container is
                // one more thing the one clock above already covers, which is
                // the argument for having put it there.
                match Worker::open(&self.spec, &self.commands).await {
                    Ok((pipe, _)) => {
                        self.name(pipe.container.clone());
                        *slot = Some(pipe);
                        self.restarts
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(error) => {
                        return self.gave_up(format!("could not be restarted: {error}"));
                    }
                }
            }
            let pipe = slot.as_mut().expect("a pipe was just put back");

            // No clock here: the loop above holds the one deadline over every
            // tool call, because it is the only thing above *both* places a
            // tool can run. What this side owns is the corpse — see
            // [`Executor::abandon`]. Being dropped mid-exchange is why the pipe
            // must never be reused afterwards: the answer may still arrive, and
            // the next call would be paired with this call's outcome, two
            // well-formed lines and nothing to say they were swapped.
            match pipe.round_trip(&request).await {
                Ok(Response::Outcome(outcome)) => *outcome,
                Ok(Response::Error { message }) => ToolOutcome::failed(
                    Verdict::deny(format!("the worker refused: {message}")),
                    message,
                ),
                Ok(Response::Hello(_)) => ToolOutcome::failed(
                    Verdict::deny("the worker greeted twice"),
                    "the worker greeted twice",
                ),
                Err(error) => {
                    // A broken pipe is a dead worker, and until now it stayed
                    // dead for the rest of the session: every later call in the
                    // turn failed on the same corpse. It is dropped here so the
                    // next call starts one.
                    *slot = None;
                    self.name(None);
                    self.gave_up(error)
                }
            }
        })
    }

    fn abandon(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.replace())
    }

    fn describe(&self) -> Option<String> {
        let mut text = format!(
            "  worker     {} — luu {}, protocol {}\n",
            self.label, self.hello.version, self.hello.protocol,
        );
        // Zero on a healthy session, and worth printing anyway: a session that
        // has replaced its worker four times is a broken image wearing the
        // costume of a slow one.
        if self.restarts() > 0 {
            text.push_str(&format!(
                "  restarts   {}   (workers this session had to replace)\n",
                self.restarts(),
            ));
        }
        if !self.hello.commands.is_empty() {
            text.push_str(&format!(
                "  on PATH    {}\n",
                match self.hello.present().is_empty() {
                    true => "(none)".to_string(),
                    false => self.hello.present().join(", "),
                }
            ));
            // The third failure mode, said before a model can find it: granted
            // by the policy, absent from the image.
            let absent = self.hello.absent();
            if !absent.is_empty() {
                text.push_str(&format!(
                    "  absent     {}   (granted by the policy, absent from the image)\n",
                    absent.join(", "),
                ));
            }
        }
        Some(text)
    }
}

impl Pipe {
    async fn round_trip(&mut self, request: &Request) -> Result<Response, String> {
        let line = serde_json::to_string(request).map_err(|error| error.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|error| error.to_string())?;
        self.stdin
            .flush()
            .await
            .map_err(|error| error.to_string())?;
        match self.read().await? {
            Some(response) => Ok(response),
            None => Err(self.epitaph().await),
        }
    }

    async fn read(&mut self) -> Result<Option<Response>, String> {
        let mut line = String::new();
        match self.stdout.read_line(&mut line).await {
            Ok(0) => Ok(None),
            Ok(_) => serde_json::from_str(line.trim())
                .map(Some)
                .map_err(|error| format!("unreadable answer ({error}): {}", line.trim())),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Ends the worker, for a worker that will not end itself.
    ///
    /// `kill_on_drop` would do it eventually, and "eventually" is not a property
    /// worth having where the thing being ended is a container: the wait is what
    /// makes the next line in the log true.
    async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Why the pipe closed, in the words of the thing that closed it. A worker
    /// that could not start says so on the exit code, and "broken pipe" is not
    /// an answer anyone can act on.
    ///
    /// With the last lines it wrote to stderr, which is where a runtime that
    /// would not start the container says why. Waited for briefly: its output
    /// closing and its exit, and the last of its stderr, are three events.
    async fn epitaph(&mut self) -> String {
        let patience = std::time::Duration::from_millis(500);
        let status = tokio::time::timeout(patience, self.child.wait()).await;
        if let Some(copying) = self.said_done.take() {
            let _ = tokio::time::timeout(patience, copying).await;
        }
        let why = match status {
            Ok(Ok(status)) => format!("the worker exited ({status})"),
            _ => "the worker closed its output".to_string(),
        };
        let said = self
            .said
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if said.is_empty() {
            why
        } else {
            let said: Vec<&str> = said.iter().map(String::as_str).collect();
            format!("{why}: {}", said.join(" · "))
        }
    }
}

/// The worker's own loop: read a call, run it, answer.
///
/// This is `Tools::standard()` and `Sandbox::new` on the far side of a pipe —
/// the same code, not a second copy of it, which is the property that makes the
/// container one enforcement point rather than two.
pub async fn serve_stdio(
    tools: Arc<Tools>,
    commands: Vec<String>,
    input: impl tokio::io::AsyncRead + Unpin,
    mut output: impl tokio::io::AsyncWrite + Unpin,
) -> std::io::Result<()> {
    let hello = Response::Hello(Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_string(),
        commands: commands
            .iter()
            .map(|name| CommandOnPath {
                name: name.clone(),
                path: which(name),
            })
            .collect(),
    });
    write_line(&mut output, &hello).await?;

    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Call { call, sandbox }) => match sandbox.resolve() {
                Ok(sandbox) => Response::Outcome(Box::new(tools.call(&call, &sandbox).await)),
                Err(error) => Response::Error {
                    message: format!("the policy did not resolve here: {error}"),
                },
            },
            Err(error) => Response::Error {
                message: format!("unreadable request ({error})"),
            },
        };
        write_line(&mut output, &response).await?;
    }
    Ok(())
}

async fn write_line(
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    response: &Response,
) -> std::io::Result<()> {
    let line = serde_json::to_string(response)?;
    output.write_all(line.as_bytes()).await?;
    output.write_all(b"\n").await?;
    output.flush().await
}

/// Where a program name resolves on this side's `PATH`, if anywhere.
///
/// Deliberately not `Command::new(name).spawn()`: the question is whether the
/// image *has* it, and running it to find out is a side effect nobody asked
/// for. Public for the page's environment picker, which asks the same
/// question about a runtime on this machine.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::path::Path::new(name);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}
