//! Model servers luu starts: `llama-server` and `ollama serve`, as children of
//! `luu serve` that die with it.
//!
//! Three questions, one section each: **which binary** (found on this machine,
//! luu's own copy, or a path the file names), **its life** (started when a
//! session needs it, stopped with the server), and **luu's own copy** (an
//! official release, by tag, checked against the digest GitHub publishes for
//! it). See `RECORD/2026-09-28.a-model-server-luu-starts.completed.md`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::Mutex;

use crate::provider::{Engine, EngineKind};

/// The releases luu downloads when the table names no `version`. Pinned rather
/// than `latest`: llama.cpp ships a build every few hours, and a copy that
/// moved under the machine is a copy nobody can compare a run against.
pub const LLAMA_VERSION: &str = "b11236";
pub const OLLAMA_VERSION: &str = "v0.34.4";
/// `mlx-serve`, the MLX engine: a native binary with a GitHub release and a
/// digest, measured against `mlx_lm.server` on this repository's M-series
/// machine and chosen over it — see the engines record.
pub const MLX_SERVE_VERSION: &str = "v26.9.6";

/// Lines of a server's output kept for the page: enough to show why it died.
const LOG_LINES: usize = 200;
const DEFAULT_READY_SECS: u64 = 120;

impl EngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::Llama => "llama",
            EngineKind::Ollama => "ollama",
            EngineKind::Mlx => "mlx",
        }
    }

    /// The executable's file name — and, from the page, the only name a path
    /// may end in. See [`accepts_from_page`].
    pub fn binary_name(self) -> &'static str {
        match (self, cfg!(windows)) {
            (EngineKind::Llama, false) => "llama-server",
            (EngineKind::Llama, true) => "llama-server.exe",
            (EngineKind::Ollama, false) => "ollama",
            (EngineKind::Ollama, true) => "ollama.exe",
            (EngineKind::Mlx, _) => "mlx-serve",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            EngineKind::Llama => 8080,
            EngineKind::Ollama => 11434,
            EngineKind::Mlx => 11234,
        }
    }

    pub fn pinned(self) -> &'static str {
        match self {
            EngineKind::Llama => LLAMA_VERSION,
            EngineKind::Ollama => OLLAMA_VERSION,
            EngineKind::Mlx => MLX_SERVE_VERSION,
        }
    }

    fn repo(self) -> &'static str {
        match self {
            EngineKind::Llama => "ggml-org/llama.cpp",
            EngineKind::Ollama => "ollama/ollama",
            EngineKind::Mlx => "ddalcu/mlx-serve",
        }
    }

    /// What answers once the model is loaded. llama-server's `/health` is 503
    /// while it loads and 200 after; ollama's `/` is 200 as soon as it listens,
    /// because it loads a model per request.
    fn health(self) -> &'static str {
        match self {
            EngineKind::Llama | EngineKind::Mlx => "/health",
            EngineKind::Ollama => "/",
        }
    }

    /// The URL a profile for this engine sends to.
    pub fn url(self, port: u16) -> String {
        match self {
            EngineKind::Llama | EngineKind::Mlx => format!("http://127.0.0.1:{port}/v1"),
            EngineKind::Ollama => format!("http://127.0.0.1:{port}"),
        }
    }

    /// The release asset for this machine, by the names each project uses.
    pub fn asset(self, version: &str, variant: Option<&str>) -> Result<String, String> {
        asset_for(
            self,
            version,
            variant,
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    }
}

fn asset_for(
    kind: EngineKind,
    version: &str,
    variant: Option<&str>,
    os: &str,
    arch: &str,
) -> Result<String, String> {
    let unknown = || format!("no {} release is published for {os}/{arch}", kind.as_str());
    match kind {
        // One asset, and one platform: MLX is Apple Silicon's. The tag is
        // not in the name.
        EngineKind::Mlx => match (os, arch, variant) {
            ("macos", "aarch64", None) => Ok("mlx-serve-bin-macos-arm64.tar.gz".into()),
            ("macos", "aarch64", Some(v)) => Err(format!("mlx-serve publishes no `{v}` build")),
            _ => Err(format!(
                "MLX runs on Apple Silicon, and this is {os}/{arch}"
            )),
        },
        EngineKind::Llama => {
            let arch = match arch {
                "aarch64" => "arm64",
                "x86_64" => "x64",
                _ => return Err(unknown()),
            };
            let (platform, ext) = match (os, variant) {
                ("macos", None) => (format!("macos-{arch}"), "tar.gz"),
                ("macos", Some(v)) => {
                    return Err(format!(
                        "llama.cpp publishes no `{v}` build for macOS: Metal is the one there is"
                    ));
                }
                ("linux", None) => (format!("ubuntu-{arch}"), "tar.gz"),
                ("linux", Some(v)) => (format!("ubuntu-{v}-{arch}"), "tar.gz"),
                ("windows", v) => (format!("win-{}-{arch}", v.unwrap_or("cpu")), "zip"),
                _ => return Err(unknown()),
            };
            Ok(format!("llama-{version}-bin-{platform}.{ext}"))
        }
        EngineKind::Ollama => {
            let suffix = variant.map(|v| format!("-{v}")).unwrap_or_default();
            match (os, arch) {
                ("macos", _) => Ok("ollama-darwin.tgz".into()),
                ("linux", "x86_64") => Ok(format!("ollama-linux-amd64{suffix}.tar.zst")),
                ("linux", "aarch64") => Ok(format!("ollama-linux-arm64{suffix}.tar.zst")),
                ("windows", "x86_64") => Ok("ollama-windows-amd64.zip".into()),
                ("windows", "aarch64") => Ok("ollama-windows-arm64.zip".into()),
                _ => Err(unknown()),
            }
        }
    }
}

// ---- which binary ---------------------------------------------------------

/// `<state dir>/engines`, beside `config.toml`.
pub fn root() -> Option<PathBuf> {
    Some(
        crate::provider::Config::path_for_writing()?
            .parent()?
            .join("engines"),
    )
}

/// Where one managed copy lives: a directory per kind, version and variant, so
/// two variants of one release do not overwrite each other.
pub fn managed_dir(root: &Path, kind: EngineKind, version: &str, variant: Option<&str>) -> PathBuf {
    let name = match variant {
        Some(variant) => format!("{version}-{variant}"),
        None => version.to_string(),
    };
    root.join(kind.as_str()).join(name)
}

/// The binary inside an unpacked release. Searched rather than assumed: the
/// two projects lay their archives out differently, and both have changed it.
pub fn find_in(dir: &Path, name: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|n| n == name) {
                return Some(path);
            }
        }
        if depth == 0 {
            return None;
        }
        dirs.sort();
        dirs.iter().find_map(|d| walk(d, name, depth - 1))
    }
    walk(dir, name, 4)
}

/// Every copy of this kind's binary on the machine: `PATH`, then the places a
/// package manager or an app puts one without adding it there.
pub fn discover(kind: EngineKind) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    if let Some(home) = &home {
        dirs.push(home.join(".local/bin"));
    }
    if kind == EngineKind::Ollama {
        dirs.push("/Applications/Ollama.app/Contents/Resources".into());
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let candidate = dir.join(kind.binary_name());
        if !is_executable(&candidate) {
            continue;
        }
        // One entry per real file: Homebrew's `bin/` is a symlink farm, and
        // the same binary reached twice is not two choices.
        let real = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if seen.insert(real) {
            found.push(candidate);
        }
    }
    found
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Whether the page may name this path as the binary. Only if it is the
/// kind's own executable by name: an engine is a process luu runs with
/// arguments, and a page that could name *any* executable is a page that runs
/// anything. The file itself may name what it likes — whoever edits it already
/// has a shell.
pub fn accepts_from_page(kind: EngineKind, binary: &str) -> bool {
    binary == "managed"
        || Path::new(binary)
            .file_name()
            .is_some_and(|name| name == kind.binary_name())
}

/// The binary an engine would run, or why there is none.
pub fn resolve_binary(engine: &Engine) -> Result<PathBuf, String> {
    let kind = engine.kind;
    match engine.binary.as_deref() {
        Some("managed") => {
            let version = engine.version.as_deref().unwrap_or(kind.pinned());
            let root = root().ok_or("this machine has no state directory yet")?;
            let dir = managed_dir(&root, kind, version, engine.variant.as_deref());
            find_in(&dir, kind.binary_name()).ok_or_else(|| {
                format!(
                    "luu's own {} {version} is not downloaded yet",
                    kind.as_str()
                )
            })
        }
        Some(path) => {
            let path = crate::provider::expand_home(Path::new(path));
            match is_executable(&path) {
                true => Ok(path),
                false => Err(format!("{} is not an executable file", path.display())),
            }
        }
        None => discover(kind).into_iter().next().ok_or_else(|| {
            format!(
                "no {} on this machine: download luu's own copy, or name a path",
                kind.binary_name()
            )
        }),
    }
}

/// The file `-m` is given: a reference out of [`crate::models`], or a path.
fn model_path(reference: &str, kind: EngineKind) -> Result<PathBuf, String> {
    if !crate::models::is_reference(reference) {
        return Ok(crate::provider::expand_home(Path::new(reference)));
    }
    let model = crate::models::resolve(reference)
        .ok_or_else(|| format!("{reference} is not on this machine"))?;
    model
        .runs_on(kind)
        .map_err(|why| format!("{}: {why}", model.label))?;
    let path = PathBuf::from(&model.path);
    // mlx-serve tells a GGUF from an MLX directory by the extension, and an
    // ollama blob has none: handed `sha256-…` it answers `NotDir`. A link
    // named `.gguf` beside the engines is what it is given instead — nothing
    // is copied. Measured with mlx-serve v26.9.6 on `qwen2.5-coder:7b`.
    if kind == EngineKind::Mlx && model.gguf() && path.extension().is_none_or(|e| e != "gguf") {
        return gguf_link(&model.reference, &path);
    }
    Ok(path)
}

fn gguf_link(reference: &str, target: &Path) -> Result<PathBuf, String> {
    let dir = root()
        .ok_or("this machine has no state directory yet")?
        .join("links");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let name: String = reference
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let link = dir.join(format!("{name}.gguf"));
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &link)
        .map_err(|e| format!("linking {}: {e}", link.display()))?;
    #[cfg(not(unix))]
    std::fs::hard_link(target, &link).map_err(|e| format!("linking {}: {e}", link.display()))?;
    Ok(link)
}

/// The engine a run on this destination needs, **on the model it asked
/// for**: a llama.cpp engine takes the session's model when that is a
/// reference into what this machine has, and its table's own otherwise. That
/// is how choosing a model in the chat moves the engine onto it.
pub fn for_run(resolved: &crate::provider::Resolved) -> Option<(String, Engine)> {
    let (name, mut engine) = resolved.engine.clone()?;
    if engine.kind != EngineKind::Ollama && crate::models::is_reference(&resolved.model) {
        engine.model = Some(resolved.model.clone());
    }
    Some((name, engine))
}

// ---- its life -------------------------------------------------------------

/// What the page is told about one engine.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// `running`, `stopped`, or `exited` — the last with `exit` saying how.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<String>,
    /// Whether something answers on the port, ours or not: an Ollama app the
    /// person already runs is a server this one does not need to start.
    pub answering: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_secs: Option<u64>,
    pub log: Vec<String>,
}

struct Running {
    child: tokio::process::Child,
    binary: PathBuf,
    started: Instant,
    /// The reference it was started on, so a session that asks for another
    /// can tell that this one is not it.
    model: Option<String>,
}

#[derive(Default)]
struct Slot {
    running: Option<Running>,
    exit: Option<String>,
    log: Arc<std::sync::Mutex<VecDeque<String>>>,
}

impl Slot {
    /// Reaps a child that exited on its own, so its status is what it did
    /// rather than what it was last seen doing.
    fn reap(&mut self) {
        if let Some(running) = &mut self.running
            && let Ok(Some(status)) = running.child.try_wait()
        {
            self.exit = Some(status.to_string());
            self.running = None;
        }
    }

    fn tail(&self, n: usize) -> Vec<String> {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.iter()
            .skip(log.len().saturating_sub(n))
            .cloned()
            .collect()
    }
}

/// One download, as the page polls it.
#[derive(Debug, Clone, Serialize)]
pub struct Install {
    pub kind: EngineKind,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub asset: String,
    /// `downloading`, `verifying`, `unpacking`, `done` or `failed`.
    pub state: &'static str,
    pub done: u64,
    pub total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The engines this server started, and the downloads it is running.
#[derive(Default)]
pub struct Supervisor {
    slots: Mutex<BTreeMap<String, Slot>>,
    installs: std::sync::Mutex<BTreeMap<String, Install>>,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("luu/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

async fn answering(kind: EngineKind, port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}{}", kind.health());
    match http().get(url).timeout(Duration::from_secs(2)).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

fn pipe(
    from: impl AsyncRead + Unpin + Send + 'static,
    log: Arc<std::sync::Mutex<VecDeque<String>>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(from).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
            if log.len() == LOG_LINES {
                log.pop_front();
            }
            log.push_back(line);
        }
    });
}

impl Supervisor {
    pub async fn status(&self, name: &str, engine: &Engine) -> Status {
        let port = engine.port.unwrap_or(engine.kind.default_port());
        let answering = answering(engine.kind, port).await;
        let mut slots = self.slots.lock().await;
        let slot = slots.entry(name.to_string()).or_default();
        slot.reap();
        Status {
            state: match (&slot.running, &slot.exit) {
                (Some(_), _) => "running",
                (None, Some(_)) => "exited",
                (None, None) => "stopped",
            },
            pid: slot.running.as_ref().and_then(|r| r.child.id()),
            exit: slot.exit.clone(),
            answering,
            binary: slot
                .running
                .as_ref()
                .map(|r| r.binary.display().to_string()),
            started_secs: slot.running.as_ref().map(|r| r.started.elapsed().as_secs()),
            log: slot.tail(40),
        }
    }

    /// Starts it unless this server already has it running.
    pub async fn start(&self, name: &str, engine: &Engine) -> Result<(), String> {
        let binary = resolve_binary(engine)?;
        let port = engine.port.unwrap_or(engine.kind.default_port());
        let mut slots = self.slots.lock().await;
        let slot = slots.entry(name.to_string()).or_default();
        slot.reap();
        if slot.running.is_some() {
            return Ok(());
        }
        let mut command = tokio::process::Command::new(&binary);
        match engine.kind {
            EngineKind::Llama => {
                command.args(["--host", "127.0.0.1", "--port", &port.to_string()]);
                if let Some(reference) = &engine.model {
                    command.arg("-m").arg(model_path(reference, engine.kind)?);
                }
            }
            EngineKind::Mlx => {
                command.args([
                    "--serve",
                    "--host",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                ]);
                if let Some(reference) = &engine.model {
                    command
                        .arg("--model")
                        .arg(model_path(reference, engine.kind)?);
                }
            }
            EngineKind::Ollama => {
                command
                    .arg("serve")
                    .env("OLLAMA_HOST", format!("127.0.0.1:{port}"));
            }
        }
        command
            .args(&engine.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // The second half of "tied to `luu serve`": the first is `stop_all`
            // on a signal, and this is every other way the handle goes away.
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| format!("starting {}: {error}", binary.display()))?;
        {
            let mut log = slot.log.lock().unwrap_or_else(|e| e.into_inner());
            log.clear();
            let model = match (&engine.kind, &engine.model) {
                (EngineKind::Llama, Some(reference)) => format!(" -m <{reference}>"),
                (EngineKind::Mlx, Some(reference)) => format!(" --model <{reference}>"),
                _ => String::new(),
            };
            log.push_back(format!(
                "$ {}{model} {}",
                binary.display(),
                engine.args.join(" ")
            ));
        }
        if let Some(out) = child.stdout.take() {
            pipe(out, slot.log.clone());
        }
        if let Some(err) = child.stderr.take() {
            pipe(err, slot.log.clone());
        }
        slot.exit = None;
        slot.running = Some(Running {
            child,
            binary,
            started: Instant::now(),
            model: engine.model.clone(),
        });
        Ok(())
    }

    /// Answering on its port, on the model asked for, within the table's
    /// `ready-timeout`. What a session on a profile with `engine` waits for,
    /// and all of it automatic:
    ///
    /// - luu's own copy is **downloaded** the first time it is needed;
    /// - an engine this server runs on **another model is restarted** on this
    ///   one — llama-server holds one model, and the session asked for this;
    /// - a server something else runs on the port is used as it is: it is not
    ///   luu's to restart.
    pub async fn ensure(self: &Arc<Self>, name: &str, engine: &Engine) -> Result<(), String> {
        let port = engine.port.unwrap_or(engine.kind.default_port());
        let ours = {
            let mut slots = self.slots.lock().await;
            let slot = slots.entry(name.to_string()).or_default();
            slot.reap();
            slot.running.as_ref().map(|r| r.model.clone())
        };
        match ours {
            Some(model) if model != engine.model && engine.kind != EngineKind::Ollama => {
                self.stop(name).await;
            }
            _ => {
                if answering(engine.kind, port).await {
                    return Ok(());
                }
            }
        }
        if engine.binary.as_deref() == Some("managed") && resolve_binary(engine).is_err() {
            self.install_and_wait(engine).await?;
        }
        self.start(name, engine).await?;
        let limit = Duration::from_secs(engine.ready_timeout.unwrap_or(DEFAULT_READY_SECS));
        let began = Instant::now();
        loop {
            if answering(engine.kind, port).await {
                return Ok(());
            }
            {
                let mut slots = self.slots.lock().await;
                let slot = slots.entry(name.to_string()).or_default();
                slot.reap();
                if slot.running.is_none() {
                    return Err(format!(
                        "engine {name} exited ({}) before it answered:\n{}",
                        slot.exit.as_deref().unwrap_or("stopped"),
                        slot.tail(8).join("\n")
                    ));
                }
            }
            if began.elapsed() > limit {
                return Err(format!(
                    "engine {name} did not answer on port {port} within {}s — `ready-timeout` \
                     in [engine.{name}] moves the limit",
                    limit.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    pub async fn stop(&self, name: &str) {
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(name)
            && let Some(mut running) = slot.running.take()
        {
            let _ = running.child.kill().await;
            slot.exit = Some("stopped from luu".into());
        }
    }

    /// Stops every engine this server started except `keep` — what happens
    /// when a session moves to a destination that does not need them, so an
    /// engine nobody sends to does not hold its model in memory until `serve`
    /// exits. A server luu did not start is never touched.
    pub async fn stop_others(&self, keep: Option<&str>) {
        let mut slots = self.slots.lock().await;
        for (name, slot) in slots.iter_mut() {
            if Some(name.as_str()) == keep {
                continue;
            }
            if let Some(mut running) = slot.running.take() {
                let _ = running.child.kill().await;
                slot.exit = Some("stopped: no session sends to it".into());
            }
        }
    }

    /// Every engine this server started. Called when `serve` is asked to stop,
    /// because a signal ends the process without running a single `Drop`.
    pub async fn stop_all(&self) {
        let mut slots = self.slots.lock().await;
        for slot in slots.values_mut() {
            if let Some(mut running) = slot.running.take() {
                let _ = running.child.kill().await;
            }
        }
    }

    // ---- luu's own copy ---------------------------------------------------

    /// Downloads luu's own copy for this engine and waits for it: the
    /// download a session triggers by needing it, shown on the Engines page
    /// like one a person started.
    async fn install_and_wait(self: &Arc<Self>, engine: &Engine) -> Result<(), String> {
        let version = engine
            .version
            .clone()
            .unwrap_or_else(|| engine.kind.pinned().to_string());
        let asset = engine.kind.asset(&version, engine.variant.as_deref())?;
        let key = format!("{}/{}", engine.kind.as_str(), asset);
        self.install(engine.kind, Some(version), engine.variant.clone())?;
        loop {
            let state = self
                .installs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .map(|i| (i.state, i.error.clone()));
            match state {
                Some(("done", _)) => return Ok(()),
                Some(("failed", error)) => {
                    return Err(error.unwrap_or_else(|| "the download failed".into()));
                }
                None => return Err("the download did not start".into()),
                _ => tokio::time::sleep(Duration::from_millis(300)).await,
            }
        }
    }

    pub fn installs(&self) -> Vec<Install> {
        self.installs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn update(&self, key: &str, change: impl FnOnce(&mut Install)) {
        if let Some(install) = self
            .installs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(key)
        {
            change(install);
        }
    }

    /// Starts a download in the background and returns at once; the page
    /// polls [`Supervisor::installs`]. One at a time per copy.
    pub fn install(
        self: &Arc<Self>,
        kind: EngineKind,
        version: Option<String>,
        variant: Option<String>,
    ) -> Result<(), String> {
        let root = root().ok_or("this machine has no state directory yet")?;
        let version = version.unwrap_or_else(|| kind.pinned().to_string());
        let asset = kind.asset(&version, variant.as_deref())?;
        let key = format!("{}/{}", kind.as_str(), asset);
        {
            let mut installs = self.installs.lock().unwrap_or_else(|e| e.into_inner());
            if installs
                .get(&key)
                .is_some_and(|i| !matches!(i.state, "done" | "failed"))
            {
                return Ok(());
            }
            installs.insert(
                key.clone(),
                Install {
                    kind,
                    version: version.clone(),
                    variant: variant.clone(),
                    asset: asset.clone(),
                    state: "downloading",
                    done: 0,
                    total: 0,
                    error: None,
                },
            );
        }
        let this = self.clone();
        tokio::spawn(async move {
            let dir = managed_dir(&root, kind, &version, variant.as_deref());
            let result = this.download(&key, kind, &version, &asset, &dir).await;
            this.update(&key, |install| match result {
                Ok(()) => install.state = "done",
                Err(error) => {
                    install.state = "failed";
                    install.error = Some(error);
                }
            });
        });
        Ok(())
    }

    async fn download(
        &self,
        key: &str,
        kind: EngineKind,
        version: &str,
        asset: &str,
        dir: &Path,
    ) -> Result<(), String> {
        use futures_util::StreamExt;
        use sha2::Digest;
        use tokio::io::AsyncWriteExt;

        #[derive(serde::Deserialize)]
        struct Release {
            assets: Vec<Asset>,
        }
        #[derive(serde::Deserialize)]
        struct Asset {
            name: String,
            browser_download_url: String,
            size: u64,
            digest: Option<String>,
        }

        let client = http();
        let release: Release = client
            .get(format!(
                "https://api.github.com/repos/{}/releases/tags/{version}",
                kind.repo()
            ))
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("asking GitHub for {} {version}: {e}", kind.repo()))?
            .json()
            .await
            .map_err(|e| format!("reading GitHub's answer: {e}"))?;
        let found = release
            .assets
            .into_iter()
            .find(|a| a.name == asset)
            .ok_or_else(|| format!("{} {version} has no asset named {asset}", kind.repo()))?;
        // No digest, no download: a file that cannot be checked is a file this
        // would be executing on the release page's word alone.
        let expected = found
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
            .ok_or("GitHub publishes no sha256 for this asset, so it cannot be checked")?
            .to_ascii_lowercase();
        self.update(key, |i| i.total = found.size);

        let parent = dir.parent().ok_or("no parent directory")?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        let part = parent.join(format!("{asset}.part"));
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| format!("creating {}: {e}", part.display()))?;
        let mut hasher = sha2::Sha256::new();
        let mut stream = client
            .get(&found.browser_download_url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("downloading {asset}: {e}"))?
            .bytes_stream();
        let mut done = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("downloading {asset}: {e}"))?;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("writing {}: {e}", part.display()))?;
            done += chunk.len() as u64;
            self.update(key, |i| i.done = done);
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file);

        self.update(key, |i| i.state = "verifying");
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != expected {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!(
                "{asset} does not match the digest GitHub publishes for it \
                 ({actual} ≠ {expected}); deleted, not unpacked"
            ));
        }

        // Unpacked beside and renamed into place, so a failed unpack never
        // leaves half a copy where `resolve_binary` would find it.
        self.update(key, |i| i.state = "unpacking");
        let staging = parent.join(format!(
            ".{}.unpack",
            dir.file_name().and_then(|n| n.to_str()).unwrap_or("copy")
        ));
        let _ = tokio::fs::remove_dir_all(&staging).await;
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|e| format!("creating {}: {e}", staging.display()))?;
        let output = tokio::process::Command::new("tar")
            .arg("-xf")
            .arg(&part)
            .arg("-C")
            .arg(&staging)
            .output()
            .await
            .map_err(|e| format!("running tar: {e}"))?;
        let _ = tokio::fs::remove_file(&part).await;
        if !output.status.success() {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            let why = String::from_utf8_lossy(&output.stderr);
            let hint = match asset.ends_with(".zst") {
                true => " — a .tar.zst needs `zstd` installed",
                false => "",
            };
            return Err(format!("unpacking {asset}: {}{hint}", why.trim()));
        }
        if find_in(&staging, kind.binary_name()).is_none() {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(format!(
                "{asset} unpacked, and there is no {} in it",
                kind.binary_name()
            ));
        }
        let _ = tokio::fs::remove_dir_all(dir).await;
        tokio::fs::rename(&staging, dir)
            .await
            .map_err(|e| format!("moving the copy into {}: {e}", dir.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names the two projects publish, checked against the release pages
    /// on 2026-09-28 — not derived from anything, so this is the test that says
    /// when one of them renames its archives.
    #[test]
    fn each_platform_gets_the_asset_its_project_publishes() {
        let llama = |os, arch, variant| asset_for(EngineKind::Llama, "b11236", variant, os, arch);
        assert_eq!(
            llama("macos", "aarch64", None).unwrap(),
            "llama-b11236-bin-macos-arm64.tar.gz"
        );
        assert_eq!(
            llama("linux", "x86_64", None).unwrap(),
            "llama-b11236-bin-ubuntu-x64.tar.gz"
        );
        assert_eq!(
            llama("linux", "x86_64", Some("vulkan")).unwrap(),
            "llama-b11236-bin-ubuntu-vulkan-x64.tar.gz"
        );
        assert_eq!(
            llama("windows", "x86_64", None).unwrap(),
            "llama-b11236-bin-win-cpu-x64.zip"
        );
        assert!(llama("macos", "aarch64", Some("cuda-12.8")).is_err());

        let mlx = |os, arch| asset_for(EngineKind::Mlx, "v26.9.6", None, os, arch);
        assert_eq!(
            mlx("macos", "aarch64").unwrap(),
            "mlx-serve-bin-macos-arm64.tar.gz"
        );
        assert!(mlx("linux", "x86_64").is_err(), "MLX is Apple Silicon's");

        let ollama =
            |os, arch, variant| asset_for(EngineKind::Ollama, "v0.34.4", variant, os, arch);
        assert_eq!(
            ollama("macos", "aarch64", None).unwrap(),
            "ollama-darwin.tgz"
        );
        assert_eq!(
            ollama("linux", "x86_64", None).unwrap(),
            "ollama-linux-amd64.tar.zst"
        );
        assert_eq!(
            ollama("linux", "x86_64", Some("rocm")).unwrap(),
            "ollama-linux-amd64-rocm.tar.zst"
        );
    }

    /// The session's model moves a llama.cpp engine when it is a reference
    /// into this machine's models, and never an ollama one, which serves them
    /// all.
    #[test]
    fn a_session_picks_the_model_its_llama_engine_loads() {
        let text = "[engine.local]\nkind = \"llama\"\nmodel = \"ollama:a:1b\"\n\n\
                    [engine.ol]\nkind = \"ollama\"\n\n\
                    [provider.local]\nbackend = \"openai\"\nengine = \"local\"\n\n\
                    [provider.ol]\nbackend = \"ollama\"\nengine = \"ol\"\n";
        let config = crate::provider::Config::from_toml(text, "config.toml").unwrap();
        let resolve = |provider, model| {
            crate::provider::resolve(
                &config,
                Some(Path::new("config.toml")),
                crate::provider::Flags {
                    provider: Some(provider),
                    model,
                    ..Default::default()
                },
            )
            .unwrap()
        };

        // Nothing asked: the table's own, and the session says so.
        let own = resolve("local", None);
        assert_eq!(own.model, "ollama:a:1b");
        assert_eq!(
            for_run(&own).unwrap().1.model.as_deref(),
            Some("ollama:a:1b")
        );
        // Another reference: the engine moves onto it.
        let other = resolve("local", Some("huggingface:o/r/f.gguf"));
        assert_eq!(
            for_run(&other).unwrap().1.model.as_deref(),
            Some("huggingface:o/r/f.gguf")
        );
        // A plain name is the server's business, not a file to load.
        let plain = resolve("local", Some("whatever"));
        assert_eq!(
            for_run(&plain).unwrap().1.model.as_deref(),
            Some("ollama:a:1b")
        );
        // ollama serves every model it has, so its engine never moves.
        let ol = resolve("ol", Some("ollama:a:1b"));
        assert_eq!(for_run(&ol).unwrap().1.model, None);
    }

    #[test]
    fn the_page_may_name_only_the_kinds_own_binary() {
        assert!(accepts_from_page(EngineKind::Llama, "managed"));
        assert!(accepts_from_page(
            EngineKind::Llama,
            "/opt/homebrew/bin/llama-server"
        ));
        assert!(accepts_from_page(EngineKind::Ollama, "~/bin/ollama"));
        assert!(!accepts_from_page(EngineKind::Llama, "/bin/sh"));
        assert!(!accepts_from_page(
            EngineKind::Ollama,
            "/usr/bin/ollama-but-not"
        ));
        assert!(!accepts_from_page(
            EngineKind::Llama,
            "/opt/homebrew/bin/ollama"
        ));
    }

    #[test]
    fn a_managed_copy_is_found_wherever_the_archive_put_it() {
        let root = std::env::temp_dir().join(format!("luu-engines-{}", std::process::id()));
        let dir = managed_dir(&root, EngineKind::Llama, "b1", None);
        std::fs::create_dir_all(dir.join("build/bin")).unwrap();
        std::fs::write(dir.join("build/bin/llama-server"), "").unwrap();
        assert_eq!(
            find_in(&dir, "llama-server"),
            Some(dir.join("build/bin/llama-server"))
        );
        assert_eq!(
            managed_dir(&root, EngineKind::Llama, "b1", Some("vulkan")),
            root.join("llama/b1-vulkan")
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
