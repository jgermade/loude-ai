//! What Settings → Runtimes shows and does: whether each container runtime is
//! installed, answers, and has the images the postures name; the tasks that
//! change that — install where Homebrew can without root, start, build an
//! image — and which policy files in the base a posture may name.
//!
//! Every line run here is one `agent_core::worker::Runtime` built — this
//! module runs them and reads what they say, and knows no runtime's API. See
//! `RECORD/2026-10-01.runtimes-in-settings.completed.md`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::worker::Runtime;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};

/// How long a probe may take before it is an answer of its own. Docker
/// Desktop's VM takes a while to wake; a runtime that is still silent after
/// this is one a session could not start on either.
const PROBE: Duration = Duration::from_secs(5);

/// The last lines of a build kept for the page: enough to read why it failed.
const TAIL: usize = 40;

/// One container runtime, as the section shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeView {
    pub runtime: String,
    /// Where its program is, or `None` when it is not installed.
    pub path: Option<String>,
    pub hint: Option<&'static str>,
    /// Whether it answered its probe. `None` where it is not installed, so
    /// nothing was asked.
    pub answers: Option<bool>,
    /// The first line it said when it did not answer.
    pub detail: Option<String>,
    /// What this runtime cannot express, where it runs anyway.
    pub gap: Option<&'static str>,
    pub images: Vec<ImageView>,
    /// The line that would install it from the page.
    pub install: Option<String>,
    /// Why the page cannot install it, where it cannot.
    pub install_refused: Option<&'static str>,
    /// Whether the page may start it: installed, not answering, and a line
    /// to start it with.
    pub startable: bool,
}

// ---- installing and starting --------------------------------------------------

/// How the page installs a runtime: Homebrew, on macOS, for the two whose
/// formula needs no administrator. `brew` refuses to run as root wherever it
/// is installed, so this is a process with the person's own authority — the
/// same one an engine's download has. See
/// `RECORD/2026-10-01.runtimes-in-settings.completed.md`.
pub fn install_steps(runtime: Runtime) -> Result<Vec<Vec<String>>, &'static str> {
    let formula = match runtime {
        Runtime::Podman => "podman",
        Runtime::Colima => "colima",
        Runtime::Docker => {
            return Err("not from here: its privileged helper needs an administrator");
        }
        Runtime::Container => {
            return Err("not from here: its package needs an administrator");
        }
        Runtime::Nerdctl => {
            return Err("not from here: it needs root");
        }
        Runtime::Host | Runtime::Direct => return Err("there is nothing to install"),
    };
    if !cfg!(target_os = "macos") {
        return Err("here the package manager needs root: install it from a terminal");
    }
    let Some(brew) = agent_core::worker::which("brew") else {
        return Err("Homebrew is not on luu serve's PATH");
    };
    Ok(vec![vec![
        brew.display().to_string(),
        "install".into(),
        formula.into(),
    ]])
}

/// How the page starts a runtime that is installed and not answering: the
/// daemon, the VM or the service, by each one's own word. `None` where that
/// takes root.
pub fn start_steps(runtime: Runtime) -> Option<Vec<Vec<String>>> {
    let line = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    match runtime {
        // The app, which starts its own VM; the daemon answers some seconds
        // after this returns.
        Runtime::Docker if cfg!(target_os = "macos") => Some(vec![line(&["open", "-a", "Docker"])]),
        // A machine is made once and started each time; `init` on one that
        // exists is an error, so it is asked first.
        Runtime::Podman if cfg!(target_os = "macos") => Some(vec![line(&[
            "sh",
            "-c",
            "podman machine inspect >/dev/null 2>&1 || podman machine init; podman machine start",
        ])]),
        Runtime::Colima => Some(vec![line(&["colima", "start", "--runtime", "containerd"])]),
        Runtime::Container => Some(vec![line(&["container", "system", "start"])]),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageView {
    pub image: String,
    /// `None` where it could not be asked: not installed, or not answering.
    pub present: Option<bool>,
}

/// Every container runtime, probed at once, each against every image in
/// `images`.
pub async fn probe_all(images: &[String]) -> Vec<RuntimeView> {
    let probes = Runtime::CONTAINED.map(|runtime| probe(runtime, images.to_vec()));
    futures_util::future::join_all(probes).await
}

async fn probe(runtime: Runtime, images: Vec<String>) -> RuntimeView {
    let program = runtime.program().unwrap_or_default();
    let path = agent_core::worker::which(program).map(|path| path.display().to_string());
    let mut view = RuntimeView {
        runtime: runtime.as_str().to_string(),
        path: path.clone(),
        hint: runtime.install_hint(),
        answers: None,
        detail: None,
        gap: runtime.cannot(),
        install: install_steps(runtime).ok().map(|steps| {
            steps
                .iter()
                .map(|step| step.join(" "))
                .collect::<Vec<_>>()
                .join(" && ")
        }),
        install_refused: install_steps(runtime).err(),
        startable: false,
        images: images
            .iter()
            .map(|image| ImageView {
                image: image.clone(),
                present: None,
            })
            .collect(),
    };
    if path.is_none() {
        return view;
    }
    let Some(argv) = runtime.probe_argv() else {
        return view;
    };
    match run(&argv).await {
        Ok(_) => view.answers = Some(true),
        Err(said) => {
            view.answers = Some(false);
            view.detail = Some(said);
            view.startable = start_steps(runtime).is_some();
            return view;
        }
    }
    for image in &mut view.images {
        if let Some(argv) = runtime.image_argv(&image.image) {
            image.present = Some(run(&argv).await.is_ok());
        }
    }
    view
}

/// Runs one line under [`PROBE`], answering with its first line of complaint
/// when it fails.
async fn run(argv: &[String]) -> Result<(), String> {
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(PROBE, command.output()).await {
        Err(_) => return Err(format!("did not answer in {}s", PROBE.as_secs())),
        Ok(Err(error)) => return Err(error.to_string()),
        Ok(Ok(output)) => output,
    };
    match output.status.success() {
        true => Ok(()),
        false => Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("it exited with an error and said nothing")
            .to_string()),
    }
}

// ---- tasks ------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    /// `build`, `install` or `start`.
    pub kind: &'static str,
    pub runtime: String,
    /// The image, for a build.
    pub image: Option<String>,
    /// `running`, `done` or `failed`.
    pub state: &'static str,
    /// The last lines it printed, both streams interleaved as they arrived.
    pub tail: Vec<String>,
    pub exit: Option<String>,
}

/// What this server ran for Settings → Runtimes, newest last: one per kind,
/// runtime and image, the latest replacing the one before it.
#[derive(Default)]
pub struct Tasks {
    all: Mutex<Vec<Arc<Mutex<TaskView>>>>,
}

fn held<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Tasks {
    pub fn list(&self) -> Vec<TaskView> {
        held(&self.all)
            .iter()
            .map(|task| held(task).clone())
            .collect()
    }

    /// Starts `runtime build` of `image` from `<base>/Containerfile`. A build
    /// of this image is minutes, because it compiles luu inside it.
    pub fn build(&self, runtime: Runtime, image: &str, base: &Path) -> Result<(), String> {
        let containerfile = base.join("Containerfile");
        if !containerfile.is_file() {
            return Err(format!(
                "there is no Containerfile in {}, which is what the image is built from",
                base.display()
            ));
        }
        let argv = runtime
            .build_argv(image, "Containerfile", ".")
            .ok_or_else(|| format!("`{runtime}` builds no images"))?;
        if agent_core::worker::which(&argv[0]).is_none() {
            return Err(format!("`{}` is not installed on this machine", argv[0]));
        }
        self.start("build", runtime, Some(image), vec![argv], base)
    }

    /// Installs it, where [`install_steps`] has a line for it.
    pub fn install(&self, runtime: Runtime, base: &Path) -> Result<(), String> {
        let steps = install_steps(runtime).map_err(str::to_string)?;
        self.start("install", runtime, None, steps, base)
    }

    /// Starts its daemon, VM or service, where [`start_steps`] has a line.
    pub fn wake(&self, runtime: Runtime, base: &Path) -> Result<(), String> {
        let steps = start_steps(runtime)
            .ok_or_else(|| format!("`{runtime}` is not started from this page: it takes root"))?;
        self.start("start", runtime, None, steps, base)
    }

    /// Runs `steps` one after another, stopping at the first that fails,
    /// unless the same task is already running.
    fn start(
        &self,
        kind: &'static str,
        runtime: Runtime,
        image: Option<&str>,
        steps: Vec<Vec<String>>,
        base: &Path,
    ) -> Result<(), String> {
        let image = image.map(str::to_string);
        let same = |task: &TaskView| {
            task.kind == kind && task.runtime == runtime.as_str() && task.image == image
        };
        let mut all = held(&self.all);
        if all.iter().any(|task| {
            let task = held(task);
            same(&task) && task.state == "running"
        }) {
            return Err(format!("that {kind} is already running on {runtime}"));
        }
        let task = Arc::new(Mutex::new(TaskView {
            kind,
            runtime: runtime.as_str().to_string(),
            image: image.clone(),
            state: "running",
            tail: Vec::new(),
            exit: None,
        }));
        all.retain(|old| !same(&held(old)));
        all.push(task.clone());
        drop(all);
        tokio::spawn(run_steps(steps, base.to_path_buf(), task));
        Ok(())
    }
}

async fn run_steps(steps: Vec<Vec<String>>, base: PathBuf, task: Arc<Mutex<TaskView>>) {
    let note = |line: String| {
        let mut task = held(&task);
        task.tail.push(line);
        let over = task.tail.len().saturating_sub(TAIL);
        task.tail.drain(..over);
    };
    let finish = |state: &'static str, exit: String| {
        let mut task = held(&task);
        task.state = state;
        task.exit = Some(exit);
    };
    let mut last = String::new();
    for argv in &steps {
        note(format!("$ {}", argv.join(" ")));
        match run_step(argv, &base, &note).await {
            Ok(status) => last = status,
            Err(status) => return finish("failed", status),
        }
    }
    finish("done", last);
    tracing::info!(target: "luu::runtimes", steps = steps.len(), "a runtime task ended");
}

async fn run_step(argv: &[String], base: &Path, note: &impl Fn(String)) -> Result<String, String> {
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(base)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for stream in [
        out.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        err.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
    }
    drop(tx);
    while let Some(line) = rx.recv().await {
        note(line);
    }
    match child.wait().await {
        Ok(status) if status.success() => Ok(status.to_string()),
        Ok(status) => Err(status.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

// ---- policy files -------------------------------------------------------------

/// A policy file a posture may name, and where it would run.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyFile {
    /// Relative to the base, the way a posture names it when `serve` runs
    /// there.
    pub path: String,
}

/// The `*.toml` files at the top of `base` that carry a `[sandbox]` or a
/// `[worker]` table. Not every TOML file: `Cargo.toml` parses as an empty
/// policy, and a list that offered it would offer a sandbox nobody wrote.
pub fn policy_files(base: &Path) -> Vec<PolicyFile> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut found: Vec<PolicyFile> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "toml"))
        .filter(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| text.parse::<toml::Table>().ok())
                .is_some_and(|table| table.contains_key("sandbox") || table.contains_key("worker"))
        })
        .filter_map(|path| {
            path.file_name().map(|name| PolicyFile {
                path: name.to_string_lossy().into_owned(),
            })
        })
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_file_is_one_that_says_so_and_cargo_toml_is_not() {
        let dir = std::env::temp_dir().join(format!("luu-policies-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("luu.toml"), "[sandbox]\nnetwork = false\n").unwrap();
        std::fs::write(dir.join("box.toml"), "[worker]\nruntime = \"docker\"\n").unwrap();
        std::fs::write(dir.join("broken.toml"), "[sandbox\n").unwrap();
        let found: Vec<String> = policy_files(&dir)
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(found, ["box.toml", "luu.toml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_runtime_that_is_not_installed_is_not_asked() {
        // Apple's is on no Linux runner and not on this Mac.
        if agent_core::worker::which("container").is_some() {
            return;
        }
        let view = probe(Runtime::Container, vec!["luu-worker:dev".into()]).await;
        assert_eq!(view.path, None);
        assert_eq!(view.answers, None, "nothing was run: {view:?}");
        assert!(view.hint.is_some());
        assert_eq!(view.images[0].present, None);
    }

    #[test]
    fn only_the_two_brew_formulas_are_installed_from_the_page() {
        for refused in [Runtime::Docker, Runtime::Container, Runtime::Nerdctl] {
            assert!(install_steps(refused).is_err(), "{refused}");
        }
        for brewed in [Runtime::Podman, Runtime::Colima] {
            match install_steps(brewed) {
                Ok(steps) => {
                    assert_eq!(steps.len(), 1);
                    assert!(
                        steps[0][0].ends_with("brew") && steps[0][1] == "install",
                        "{steps:?}"
                    );
                }
                // Linux, or no Homebrew: said, not guessed.
                Err(why) => assert!(!why.is_empty()),
            }
        }
        // nerdctl's containerd is a system service; nothing starts it here.
        assert!(start_steps(Runtime::Nerdctl).is_none());
        assert_eq!(
            start_steps(Runtime::Colima).unwrap()[0],
            ["colima", "start", "--runtime", "containerd"]
        );
    }

    #[tokio::test]
    async fn a_task_runs_its_steps_in_order_and_stops_at_the_first_that_fails() {
        let tasks = Tasks::default();
        let dir = std::env::temp_dir();
        let sh = |script: &str| vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        let steps = vec![sh("echo one"), sh("echo two; exit 3"), sh("echo three")];
        tasks
            .start("start", Runtime::Podman, None, steps, &dir)
            .expect("started");
        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let task = tasks.list().pop().expect("a task");
                if task.state != "running" {
                    return task;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("it ended");
        assert_eq!(ended.state, "failed");
        assert!(ended.tail.contains(&"one".to_string()) && ended.tail.contains(&"two".to_string()));
        assert!(
            !ended.tail.contains(&"three".to_string()),
            "{:?}",
            ended.tail
        );
    }

    #[test]
    fn a_build_needs_a_containerfile_and_an_installed_runtime() {
        let tasks = Tasks::default();
        let dir = std::env::temp_dir().join(format!("luu-build-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory");
        let refused = tasks
            .build(Runtime::Docker, "luu-worker:dev", &dir)
            .unwrap_err();
        assert!(refused.contains("no Containerfile"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
