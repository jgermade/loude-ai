//! Which container runtime, as a name rather than an integration.
//!
//! The whole of this layer is *building an argv*. That is the same shape the
//! policy already has — `commands` is an allowlist of program names, not a
//! plugin per program — and it is what makes Docker, Podman, `nerdctl` and
//! Apple's `container` substitute for each other. The dependency becomes a
//! choice, which is local-first applied to runtimes instead of to models.
//!
//! Two things this layer must do rather than assume, because the flags are
//! **not** uniform (`RECORD/2026-09-01.the-container-decided.completed.md` found the
//! gap): declare what it requires, and say what is missing when a runtime
//! cannot express it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::sandbox::{PathRule, PolicyError};

/// Where the tools of a session run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Runtime {
    /// In this process. Every run that has happened in this repository so far,
    /// and the default, so nothing changes for anyone who does not type a line.
    #[default]
    Host,
    /// A `luu worker` child process on this machine, with no container.
    ///
    /// **It isolates nothing, and it is here on purpose**: it is what makes the
    /// seam testable where there is no runtime installed — which is CI — and it
    /// answers "is it the IPC or is it the container" in one flag. It is not
    /// silently weaker: every verdict for the session names it as what it is.
    Direct,
    Docker,
    Podman,
    Nerdctl,
    /// Apple's `container`, on macOS 26 and Apple silicon.
    Container,
    /// Colima with containerd (`colima nerdctl`).
    Colima,
}

impl Runtime {
    /// Every runtime that puts a container around the worker, in the order a
    /// picker offers them. They build the same argv from the same image, so a
    /// contained posture runs on any of them; `host` and `direct` are not here
    /// because a posture's `[[worker.paths]]` exist only inside the image.
    pub const CONTAINED: [Runtime; 5] = [
        Self::Docker,
        Self::Podman,
        Self::Nerdctl,
        Self::Colima,
        Self::Container,
    ];

    /// The program this runtime is invoked as, or `None` when there is no
    /// program because there is no container.
    pub fn program(self) -> Option<&'static str> {
        match self {
            Self::Host | Self::Direct => None,
            Self::Docker => Some("docker"),
            Self::Podman => Some("podman"),
            Self::Nerdctl => Some("nerdctl"),
            Self::Container => Some("container"),
            Self::Colima => Some("colima"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Direct => "direct",
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Nerdctl => "nerdctl",
            Self::Container => "container",
            Self::Colima => "colima",
        }
    }

    /// Whether tools run in another process at all. `Host` is the one that
    /// does not, and it is the one that needs no worker.
    pub fn is_worker(self) -> bool {
        self != Self::Host
    }

    /// Whether this runtime puts a container around the worker. `Direct` does
    /// not, which is the whole of what makes it weaker and is why every line
    /// that reports it says so.
    pub fn is_contained(self) -> bool {
        self.program().is_some()
    }

    /// How this runtime is invoked, before its subcommand: its program, and
    /// colima's `nerdctl --` after it. Empty where there is no container.
    pub fn invocation(self) -> Vec<String> {
        match (self, self.program()) {
            (_, None) => Vec::new(),
            (Self::Colima, Some(program)) => {
                vec![program.to_string(), "nerdctl".to_string(), "--".to_string()]
            }
            (_, Some(program)) => vec![program.to_string()],
        }
    }

    fn with(self, rest: &[&str]) -> Option<Vec<String>> {
        let mut argv = self.invocation();
        if argv.is_empty() {
            return None;
        }
        argv.extend(rest.iter().map(|word| word.to_string()));
        Some(argv)
    }

    /// The line that asks whether the runtime answers — the daemon, the VM,
    /// the machine — rather than only whether its program is installed.
    /// Apple's has no `info`; it has `system status`.
    pub fn probe_argv(self) -> Option<Vec<String>> {
        match self {
            Self::Container => self.with(&["system", "status"]),
            _ => self.with(&["info"]),
        }
    }

    /// The line that asks whether this runtime's store has `image`. Each
    /// runtime keeps its own: Docker's images are not Podman's.
    pub fn image_argv(self, image: &str) -> Option<Vec<String>> {
        self.with(&["image", "inspect", image])
    }

    /// The line that builds `image` from `containerfile`, with `context` as
    /// the build context. One spelling for all five, which each of them
    /// documents.
    pub fn build_argv(
        self,
        image: &str,
        containerfile: &str,
        context: &str,
    ) -> Option<Vec<String>> {
        self.with(&["build", "-t", image, "-f", containerfile, context])
    }

    /// How a person gets this runtime, as text to copy. luu installs none of
    /// them: each is a package with its own installer, and on Linux, root.
    pub fn install_hint(self) -> Option<&'static str> {
        match self {
            Self::Host | Self::Direct => None,
            Self::Docker => Some(
                "Docker Desktop (docs.docker.com/get-docker), or `brew install colima docker` \
                 and `colima start`",
            ),
            Self::Podman => {
                Some("`brew install podman`, then `podman machine init && podman machine start`")
            }
            Self::Nerdctl => Some(
                "containerd and nerdctl (github.com/containerd/nerdctl/releases); on macOS, use colima",
            ),
            Self::Colima => Some("`brew install colima`, then `colima start --runtime containerd`"),
            Self::Container => Some(
                "Apple's container (github.com/apple/container/releases), macOS 26 on Apple \
                 silicon, then `container system start`",
            ),
        }
    }

    /// What this runtime cannot express, named rather than silently skipped.
    ///
    /// Apple's `container` documents `--network <net>` and `--no-dns` and has
    /// no `--network none` equivalent. Under it the container stays attached
    /// and the per-spawn seccomp filter is doing all of the denying — which is
    /// survivable, because the filter is what actually denies, and reportable,
    /// which is the part that matters.
    pub fn cannot(self) -> Option<&'static str> {
        match self {
            Self::Container => Some(
                "no --network none: the container stays attached and the \
                 per-command seccomp filter is the only thing denying the network",
            ),
            _ => None,
        }
    }
}

impl std::str::FromStr for Runtime {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "host" => Ok(Self::Host),
            "direct" => Ok(Self::Direct),
            "docker" => Ok(Self::Docker),
            "podman" => Ok(Self::Podman),
            "nerdctl" => Ok(Self::Nerdctl),
            "container" => Ok(Self::Container),
            "colima" => Ok(Self::Colima),
            other => Err(format!(
                "unknown worker runtime `{other}`: \
                 host, direct, docker, podman, nerdctl, container or colima"
            )),
        }
    }
}

impl std::fmt::Display for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("worker runtime `{runtime}` needs an image: set [worker] image, or --worker-image")]
    NoImage { runtime: Runtime },
    #[error("`{0}` runs no worker")]
    NoWorker(Runtime),
    #[error("the path to this binary could not be resolved, so no worker can be started: {0}")]
    NoBinary(#[source] std::io::Error),
}

/// Everything needed to start one worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSpec {
    pub runtime: Runtime,
    /// The image reference, for the runtimes that take one.
    pub image: Option<String>,
    /// The directory mounted into the container **at the same absolute path**,
    /// and the worker's working directory. Not `/workspace`: paths appear in
    /// verdicts, in prompts, in tool results and in the record, and a mount
    /// point that rewrote all of them would make a contained run and a host run
    /// of the same script differ in their bytes. "One flag apart" is the
    /// sentence the measurement discipline rests on.
    pub base: PathBuf,
    /// Whether the session's policy allows the network.
    ///
    /// It decides how the container is *created* and is never changed
    /// afterwards: `--network none` when the session denies it, attached when
    /// it allows it. Nothing toggles a running container's network — that is
    /// the per-command seccomp filter's job, and its window lines up with the
    /// task that asked for it in a way a `docker network disconnect` never
    /// could.
    pub network: bool,
    /// Where `luu` is, for [`Runtime::Direct`]. Inside an image it is on `PATH`
    /// by its own name.
    pub binary: Option<PathBuf>,
    /// Trees that exist only inside the image, from `[[worker.paths]]`. Never
    /// resolved on this side; added to every sandbox that crosses the pipe.
    pub paths: Vec<PathRule>,
    /// How long the seam waits for a call that has **no clock of its own**,
    /// before the worker is treated as stuck and killed. See
    /// [`WorkerSpec::patience`].
    pub timeout_ms: u64,
    /// The uid and gid the worker runs as, for the runtimes that take them.
    ///
    /// Not cosmetic. The base is bind-mounted, so a worker running as root
    /// writes root-owned files into the person's own checkout — and `writes` in
    /// an approved plan is the whole point of the mount. Defaults to whoever
    /// started the session, which is the only answer that leaves the tree the
    /// way it was found.
    pub user: Option<(u32, u32)>,
    /// `HOME` inside the container: whoever started the session's own, for
    /// the same reason the base keeps its path. `~` then means one directory
    /// in a contained run and a host run alike, and a path under it — the
    /// base, usually — is spelled the same on both sides, in a command and in
    /// the terminal's prompt. The directory exists in the container, made by
    /// the runtime as the mount's parent, and is not writable there, which is
    /// what the `/` it replaces was too.
    pub home: Option<PathBuf>,
}

impl WorkerSpec {
    pub fn new(runtime: Runtime, base: impl Into<PathBuf>) -> Self {
        Self {
            runtime,
            image: None,
            base: base.into(),
            network: false,
            binary: None,
            paths: Vec::new(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            user: current_user(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    /// What the seam adds to a call's own clock before it gives up on it.
    ///
    /// One number for "a tool with no clock of its own", answered once: a
    /// `read_file` gets this and nothing else, and a `run_command` gets this on
    /// top of the `timeout_ms` it asked for — so the seam cannot fire before the
    /// tool's own timeout has had its chance. See
    /// `RECORD/2026-09-05.a-clock-at-the-seam.completed.md`.
    pub fn patience(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    pub fn with_image(mut self, image: Option<String>) -> Self {
        self.image = image;
        self
    }

    pub fn with_network(mut self, network: bool) -> Self {
        self.network = network;
        self
    }

    pub fn with_binary(mut self, binary: Option<PathBuf>) -> Self {
        self.binary = binary;
        self
    }

    pub fn with_paths(mut self, paths: Vec<PathRule>) -> Self {
        self.paths = paths;
        self
    }

    pub fn with_user(mut self, user: Option<(u32, u32)>) -> Self {
        self.user = user;
        self
    }

    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    /// How a verdict and `luu tools` name this worker.
    pub fn label(&self) -> String {
        match (self.runtime.is_contained(), &self.image) {
            (true, Some(image)) => format!("{} ({image})", self.runtime),
            (true, None) => self.runtime.to_string(),
            (false, _) => format!("{} (no container)", self.runtime),
        }
    }

    /// The command line that starts the worker, with no container name.
    pub fn argv(&self, commands: &[String]) -> Result<Vec<String>, RuntimeError> {
        self.argv_as(commands, None)
    }

    /// The command line that starts the worker.
    ///
    /// The container's only process **is** the worker, rather than a keep-alive
    /// that is later `exec`'d into: then the container's lifetime is the
    /// worker's, there is no `docker rm` to forget, and there is no way to leave
    /// a container running after the session that owned it died.
    ///
    /// `name` is what a terminal `exec`s into, and it is a label rather than a
    /// handle: `--rm` still removes the container, name and all, when the
    /// worker's stdin closes. See
    /// `RECORD/2026-10-01.a-terminal-in-the-container.completed.md`. Ignored where
    /// there is no container.
    ///
    /// `commands` is passed through to the worker so its handshake can answer
    /// which of them the image actually has.
    pub fn argv_as(
        &self,
        commands: &[String],
        name: Option<&str>,
    ) -> Result<Vec<String>, RuntimeError> {
        let base = self.base.display().to_string();
        let worker_args = |program: String| {
            let mut argv = vec![program, "worker".to_string()];
            for command in commands {
                argv.push("--command".to_string());
                argv.push(command.clone());
            }
            argv
        };

        let Some(program) = self.runtime.program() else {
            return match self.runtime {
                Runtime::Host => Err(RuntimeError::NoWorker(Runtime::Host)),
                _ => {
                    let binary = match &self.binary {
                        Some(path) => path.clone(),
                        None => std::env::current_exe().map_err(RuntimeError::NoBinary)?,
                    };
                    Ok(worker_args(binary.display().to_string()))
                }
            };
        };

        let Some(image) = self.image.clone() else {
            return Err(RuntimeError::NoImage {
                runtime: self.runtime,
            });
        };

        let mut argv = match self.runtime {
            Runtime::Colima => vec![
                program.to_string(),
                "nerdctl".to_string(),
                "--".to_string(),
                "run".to_string(),
                "--rm".to_string(),
            ],
            _ => vec![program.to_string(), "run".to_string(), "--rm".to_string()],
        };
        // `--init` reaps what a build leaves behind. Apple's runtime does not
        // document it, so it is asked for where it exists and skipped where it
        // does not, rather than assumed uniform.
        if self.runtime != Runtime::Container {
            argv.push("--init".to_string());
        }
        if let Some(name) = name {
            argv.extend(["--name".to_string(), name.to_string()]);
        }
        argv.extend([
            "-i".to_string(),
            "--mount".to_string(),
            format!("type=bind,source={base},target={base}"),
            "--workdir".to_string(),
            base,
        ]);
        // Fixed for the container's whole life, and only where the runtime can
        // say it. `Runtime::cannot` is what reports the gap where it cannot.
        if !self.network && self.runtime.cannot().is_none() {
            argv.push("--network".to_string());
            argv.push("none".to_string());
        }
        // Two spellings of one idea, which is this layer's whole reason for
        // existing: Docker, Podman and nerdctl take `--user uid:gid`, Apple's
        // `container` takes `--uid` and `--gid` separately.
        if let Some((uid, gid)) = self.user {
            match self.runtime {
                Runtime::Container => argv.extend([
                    "--uid".to_string(),
                    uid.to_string(),
                    "--gid".to_string(),
                    gid.to_string(),
                ]),
                _ => argv.extend(["--user".to_string(), format!("{uid}:{gid}")]),
            }
        }
        if let Some(home) = &self.home {
            argv.extend(["-e".to_string(), format!("HOME={}", home.display())]);
        }
        argv.push(image);
        argv.extend(worker_args("luu".to_string()));
        Ok(argv)
    }
}

/// A shell in a worker's container: the line that opens it, and the line that
/// ends it.
///
/// **Two lines because one is not enough.** Killing the `exec` client does
/// not end what it started: the runtime keeps the process running in the
/// container, which here lives as long as the session, so every closed
/// terminal would leave a shell behind. Each shell carries
/// `LUU_TERMINAL=<id>` in its environment, which its children inherit, and
/// `end` is a second `exec` that hangs up on every process carrying it, then
/// kills what is left a second later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLine {
    pub open: Vec<String>,
    pub end: Vec<String>,
}

impl WorkerSpec {
    /// `<runtime> exec` (colima's with its `nerdctl --`), or `None` where
    /// there is no container.
    fn exec_prefix(&self) -> Option<Vec<String>> {
        let program = self.runtime.program()?;
        Some(match self.runtime {
            Runtime::Colima => vec![
                program.to_string(),
                "nerdctl".to_string(),
                "--".to_string(),
                "exec".to_string(),
            ],
            _ => vec![program.to_string(), "exec".to_string()],
        })
    }

    /// The shell a terminal opens in the named container, and how it is
    /// ended, or `None` where there is no container to open one in.
    ///
    /// Nothing about the container is restated: `exec` inherits the run's
    /// user, working directory, mounts and network, so the shell is held to
    /// exactly what the worker is held to. `bash` where the image has it,
    /// `sh` where it does not. `TERM` is named because `exec -t` sets a plain
    /// `xterm`, and the page draws 256 colours. `id` is what [`TerminalLine`]
    /// finds the shell by. The prompt is [`SHELL`]'s, and `LUU_RUNTIME` is the
    /// host it names.
    pub fn exec_argv(&self, name: &str, id: &str) -> Option<TerminalLine> {
        let mut open = self.exec_prefix()?;
        open.extend([
            "-i".to_string(),
            "-t".to_string(),
            "-e".to_string(),
            "TERM=xterm-256color".to_string(),
            "-e".to_string(),
            format!("LUU_TERMINAL={id}"),
            "-e".to_string(),
            format!("LUU_RUNTIME={}", self.runtime),
            name.to_string(),
            "sh".to_string(),
            "-c".to_string(),
            SHELL.to_string(),
        ]);
        // `/proc/<pid>/environ` is readable for the worker's own uid, which is
        // every process a terminal started. The script that reads it does not
        // carry the id, so it never signals itself.
        let each = |signal: &str| {
            format!(
                "for p in /proc/[0-9]*; do grep -qzx 'LUU_TERMINAL={id}' $p/environ 2>/dev/null \
                 && kill -{signal} ${{p#/proc/}} 2>/dev/null; done"
            )
        };
        let mut end = self.exec_prefix()?;
        end.extend([
            name.to_string(),
            "sh".to_string(),
            "-c".to_string(),
            format!("{}; sleep 1; {}; true", each("HUP"), each("KILL")),
        ]);
        Some(TerminalLine { open, end })
    }
}

/// The line a terminal's shell is started by: `bash` where the image has it,
/// `sh` where it does not, and in `bash` a prompt that reads like the one
/// luu's author has in zsh — an empty line to set it off from the output
/// above, who and where in grey on a line of its own, then the directory in
/// yellow, the branch on blue with `⚡` when the tree has changes, and `▸`:
///
/// ```text
///
/// luu@docker
/// ~/dev/luu  main ⚡  ▸
/// ```
///
/// The `~` is the host's: [`WorkerSpec::home`] is passed in as `HOME`.
///
/// The rc comes in on fd 3 rather than as a file, because the container is
/// not ours to write to. It runs after the image's own `/etc/bash.bashrc` and
/// `~/.bashrc`, so their aliases stay and only the prompt is replaced. `git`
/// is asked with `safe.directory=*` because the base is the host's and the
/// shell runs as whatever uid the run named; a git that refused would only
/// leave the branch out.
const SHELL: &str = r#"command -v bash >/dev/null 2>&1 || exec sh
exec bash --rcfile /dev/fd/3 3<<'RC'
exec 3<&-
[ -f ~/.bashrc ] && . ~/.bashrc
__luu_branch() {
  local b dirty
  b=$(git -c safe.directory='*' branch --show-current 2>/dev/null)
  [ -n "$b" ] || return 0
  [ -n "$(git -c safe.directory='*' status --porcelain 2>/dev/null)" ] && dirty=' ⚡'
  printf ' \001\e[44m\002 %s%s \001\e[49m\002' "$b" "$dirty"
}
PS1='\n\[\e[90m\]luu@${LUU_RUNTIME:-container}\[\e[39m\]\n\[\e[33m\]\w\[\e[39m\]$(__luu_branch) ▸ '
RC
"#;

/// A container name nothing else on this machine is using: one per worker
/// *start*, not per session, because a replaced worker's container is removed
/// by `--rm` asynchronously and a second start under its name would race that
/// removal.
pub fn container_name() -> String {
    static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("luu-worker-{}-{n}", std::process::id())
}

/// Whoever started this session, so the worker writes as them.
#[cfg(unix)]
fn current_user() -> Option<(u32, u32)> {
    // SAFETY: `getuid` and `getgid` take no arguments, touch no memory and
    // cannot fail.
    unsafe { Some((libc::getuid(), libc::getgid())) }
}

#[cfg(not(unix))]
fn current_user() -> Option<(u32, u32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(runtime: Runtime) -> WorkerSpec {
        WorkerSpec::new(runtime, "/home/you/project")
            .with_image(Some("luu-worker:dev".into()))
            .with_user(Some((501, 20)))
            .with_home(Some("/home/you".into()))
    }

    #[test]
    fn a_runtime_is_an_argv_and_nothing_else() {
        let argv = spec(Runtime::Docker).argv(&["cargo".to_string()]).unwrap();
        assert_eq!(
            argv,
            [
                "docker",
                "run",
                "--rm",
                "--init",
                "-i",
                "--mount",
                "type=bind,source=/home/you/project,target=/home/you/project",
                "--workdir",
                "/home/you/project",
                "--network",
                "none",
                "--user",
                "501:20",
                "-e",
                "HOME=/home/you",
                "luu-worker:dev",
                "luu",
                "worker",
                "--command",
                "cargo",
            ]
        );
    }

    #[test]
    fn the_base_is_mounted_where_it_lives_so_two_runs_can_be_diffed() {
        // Not /workspace. A mount point that rewrote every path would make a
        // contained run and a host run of the same script differ in their
        // bytes, which is more than one flag apart.
        let argv = spec(Runtime::Podman).argv(&[]).unwrap();
        let mount = argv.iter().position(|word| word == "--mount").unwrap();
        assert_eq!(
            argv[mount + 1],
            "type=bind,source=/home/you/project,target=/home/you/project"
        );
        assert_eq!(
            argv[0], "podman",
            "the runtime is the program, nothing else"
        );
    }

    #[test]
    fn a_session_that_allows_the_network_gets_a_container_that_has_one() {
        let argv = spec(Runtime::Docker).with_network(true).argv(&[]).unwrap();
        assert!(
            !argv.iter().any(|word| word == "--network"),
            "the container is created attached and never changed afterwards: {argv:?}"
        );
    }

    #[test]
    fn a_runtime_that_cannot_deny_the_network_says_so_instead_of_pretending() {
        // Apple's `container` has no --network none. The seccomp filter is
        // still what denies a command the network; what must not happen is a
        // flag being silently dropped and the verdict reading as if it held.
        let apple = spec(Runtime::Container);
        let argv = apple.argv(&[]).unwrap();
        assert!(!argv.iter().any(|word| word == "--network"));
        assert!(Runtime::Container.cannot().is_some());
        assert!(Runtime::Docker.cannot().is_none());
    }

    #[test]
    fn direct_runs_the_binary_and_names_itself_as_uncontained() {
        // No container, so no --user either: it is already this user.
        let argv = WorkerSpec::new(Runtime::Direct, "/tmp")
            .with_binary(Some("/usr/local/bin/luu".into()))
            .argv(&["git".to_string()])
            .unwrap();
        assert_eq!(argv, ["/usr/local/bin/luu", "worker", "--command", "git"]);
        assert!(!Runtime::Direct.is_contained());
        assert!(Runtime::Direct.is_worker());
        assert_eq!(
            WorkerSpec::new(Runtime::Direct, "/tmp").label(),
            "direct (no container)",
            "a mode that isolates nothing must not read as one that does"
        );
    }

    #[test]
    fn a_contained_runtime_without_an_image_is_an_error_rather_than_a_guess() {
        let error = WorkerSpec::new(Runtime::Docker, "/tmp")
            .argv(&[])
            .unwrap_err();
        assert!(error.to_string().contains("needs an image"), "{error}");
        // And `host` has no worker to start at all.
        assert!(!Runtime::Host.is_worker());
        assert!(
            WorkerSpec::new(Runtime::Host, "/tmp").argv(&[]).is_err(),
            "host is the absence of a worker, not a worker with no flags"
        );
    }

    #[test]
    fn the_worker_writes_as_whoever_started_the_session() {
        // The base is bind-mounted, so a worker running as root leaves
        // root-owned files in the person's own checkout — and `writes` in an
        // approved plan is what the mount is for.
        let docker = spec(Runtime::Docker).argv(&[]).unwrap();
        let user = docker.iter().position(|word| word == "--user").unwrap();
        assert_eq!(docker[user + 1], "501:20");

        // The same idea, spelled the way this runtime spells it. Assuming the
        // flags are uniform is the mistake this layer exists to not make.
        let apple = spec(Runtime::Container).argv(&[]).unwrap();
        assert!(!apple.iter().any(|word| word == "--user"));
        let uid = apple.iter().position(|word| word == "--uid").unwrap();
        assert_eq!(apple[uid + 1], "501");
        let gid = apple.iter().position(|word| word == "--gid").unwrap();
        assert_eq!(apple[gid + 1], "20");
    }

    #[test]
    fn the_container_s_home_is_the_host_s_so_a_tilde_means_one_directory() {
        let argv = spec(Runtime::Docker).argv(&[]).unwrap();
        assert!(argv.contains(&"HOME=/home/you".to_string()), "{argv:?}");
        // Said once, at `run`: an `exec` inherits it.
        let line = spec(Runtime::Docker).exec_argv("n", "1").unwrap();
        assert!(
            !line.open.iter().any(|word| word.starts_with("HOME=")),
            "{:?}",
            line.open
        );
        assert!(
            line.open.contains(&"LUU_RUNTIME=docker".to_string()),
            "{:?}",
            line.open
        );
        // No home known, nothing said, and the image's own stands.
        let none = spec(Runtime::Docker).with_home(None).argv(&[]).unwrap();
        assert!(
            !none.iter().any(|word| word.starts_with("HOME=")),
            "{none:?}"
        );
    }

    #[test]
    fn colima_uses_nerdctl_subcommand_with_double_dash() {
        let argv = spec(Runtime::Colima).argv(&["cargo".to_string()]).unwrap();
        assert_eq!(
            argv,
            [
                "colima",
                "nerdctl",
                "--",
                "run",
                "--rm",
                "--init",
                "-i",
                "--mount",
                "type=bind,source=/home/you/project,target=/home/you/project",
                "--workdir",
                "/home/you/project",
                "--network",
                "none",
                "--user",
                "501:20",
                "-e",
                "HOME=/home/you",
                "luu-worker:dev",
                "luu",
                "worker",
                "--command",
                "cargo",
            ]
        );
    }

    #[test]
    fn a_named_container_is_still_removed_and_named_before_the_image() {
        let argv = spec(Runtime::Docker)
            .argv_as(&[], Some("luu-worker-1-0"))
            .unwrap();
        let at = argv.iter().position(|a| a == "--name").expect("named");
        assert_eq!(argv[at + 1], "luu-worker-1-0");
        assert!(argv.contains(&"--rm".to_string()), "{argv:?}");
        let image = argv.iter().position(|a| a == "luu-worker:dev").unwrap();
        assert!(
            at < image,
            "a flag after the image is the worker's argument: {argv:?}"
        );
        // And a name is never invented where nobody asked for one.
        assert!(
            !spec(Runtime::Docker)
                .argv(&[])
                .unwrap()
                .contains(&"--name".to_string())
        );
    }

    #[test]
    fn a_terminal_execs_into_the_container_and_restates_nothing_about_it() {
        let line = spec(Runtime::Docker)
            .exec_argv("luu-worker-1-0", "7")
            .unwrap();
        let argv = line.open;
        assert_eq!(&argv[..4], &["docker", "exec", "-i", "-t"]);
        assert!(argv.contains(&"LUU_TERMINAL=7".to_string()), "{argv:?}");
        // Ended by what it was tagged with, in the same container.
        assert_eq!(&line.end[..3], &["docker", "exec", "luu-worker-1-0"]);
        assert!(line.end[5].contains("LUU_TERMINAL=7"), "{:?}", line.end);
        assert!(argv.contains(&"luu-worker-1-0".to_string()));
        // The run already said these; saying them again could only disagree.
        for flag in ["--user", "--mount", "--network", "--workdir"] {
            assert!(!argv.contains(&flag.to_string()), "{flag} in {argv:?}");
        }
        let colima = spec(Runtime::Colima).exec_argv("n", "1").unwrap();
        assert_eq!(&colima.open[..4], &["colima", "nerdctl", "--", "exec"]);
        assert_eq!(&colima.end[..4], &["colima", "nerdctl", "--", "exec"]);
        // No container, no terminal.
        assert!(spec(Runtime::Direct).exec_argv("n", "1").is_none());
        assert!(spec(Runtime::Host).exec_argv("n", "1").is_none());
    }

    #[test]
    fn a_runtime_is_probed_inspected_and_built_by_its_own_words() {
        assert_eq!(Runtime::Docker.probe_argv().unwrap(), ["docker", "info"]);
        assert_eq!(
            Runtime::Container.probe_argv().unwrap(),
            ["container", "system", "status"]
        );
        assert_eq!(
            Runtime::Colima.image_argv("luu-worker:dev").unwrap(),
            [
                "colima",
                "nerdctl",
                "--",
                "image",
                "inspect",
                "luu-worker:dev"
            ]
        );
        assert_eq!(
            Runtime::Podman
                .build_argv("luu-worker:dev", "Containerfile", ".")
                .unwrap(),
            [
                "podman",
                "build",
                "-t",
                "luu-worker:dev",
                "-f",
                "Containerfile",
                "."
            ]
        );
        // No container, no lines and no hint.
        for none in [Runtime::Host, Runtime::Direct] {
            assert!(none.probe_argv().is_none() && none.install_hint().is_none());
        }
        for each in Runtime::CONTAINED {
            assert!(each.install_hint().is_some(), "{each}");
        }
    }

    #[test]
    fn every_start_gets_a_name_of_its_own() {
        assert_ne!(container_name(), container_name());
    }

    #[test]
    fn a_runtime_name_round_trips_and_an_unknown_one_lists_the_known_ones() {
        for runtime in [
            Runtime::Host,
            Runtime::Direct,
            Runtime::Docker,
            Runtime::Podman,
            Runtime::Nerdctl,
            Runtime::Container,
            Runtime::Colima,
        ] {
            assert_eq!(runtime.as_str().parse::<Runtime>().unwrap(), runtime);
        }
        let error = "lxc".parse::<Runtime>().unwrap_err();
        assert!(error.contains("nerdctl"), "{error}");
        assert!(error.contains("colima"), "{error}");
    }
}

/// The `[worker]` block of `luu.toml`.
///
/// It sits beside `[sandbox]` rather than inside it because it answers a
/// different question: `[sandbox]` is *what the agent may reach*, and this is
/// *where the thing reaching it runs*. A policy is the same policy whichever
/// side of the pipe resolves it — which is the property the whole seam rests
/// on — so folding this into `[sandbox]` would have made the two look like one
/// decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WorkerConfig {
    /// `host` by default: no worker, and the run every one of this
    /// repository's measurements was made under.
    #[serde(default)]
    pub runtime: Runtime,
    /// The image, for the runtimes that take one. Declared rather than
    /// generated: see `Containerfile`.
    #[serde(default)]
    pub image: Option<String>,
    /// Trees that exist **only inside the image**, added to the policy the
    /// worker resolves and never resolved here.
    ///
    /// They cannot go in `[sandbox]`, and the reason is a rule worth keeping:
    /// a granted path that is not there is a load error rather than a rule that
    /// silently does nothing. `/usr/local/cargo` is the image's toolchain and
    /// there is no such directory on a Mac, so a `[sandbox]` naming it would
    /// refuse to load on the host that is meant to start the container.
    ///
    /// They behave like [`crate::sandbox::SYSTEM_ROOTS`] in what they are for —
    /// the furniture a command needs in order to be able to run at all — and
    /// unlike them in being declared rather than fixed, so they grant exactly
    /// what they say to every tool, on the far side only.
    #[serde(default)]
    pub paths: Vec<PathRule>,
    /// `[worker] timeout-ms`: how long the seam waits for a call that has no
    /// clock of its own. Default 30 000 — deliberately the same number
    /// `run_command` defaults to, because it is an answer to the same question.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// The seam's patience, and `run_command`'s default, are one number: "how long
/// may a thing with no clock take before it is stuck".
pub const DEFAULT_TIMEOUT_MS: u64 = crate::tools::command::DEFAULT_TIMEOUT_MS;

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// Derived `Default` would put a **zero** in `timeout_ms`, which is a seam that
/// kills every worker on its first call. The one field that cannot take the
/// language's default is the reason this is written out.
impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            runtime: Runtime::default(),
            image: None,
            paths: Vec::new(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// The file `luu.toml` is, read for its `[worker]` block. The `[sandbox]` half
/// is [`crate::sandbox::SandboxPolicy::from_file`]'s, and the two are separate
/// readers of one file for the same reason they are separate blocks.
#[derive(Debug, Clone, Default, Deserialize)]
struct WorkerFile {
    #[serde(default)]
    worker: Option<WorkerConfig>,
}

impl WorkerConfig {
    /// Reads `[worker]`. A file without one is not an error — it is a
    /// `luu.toml` from before this existed, and the default is `host`.
    pub fn from_file(path: &Path) -> Result<Self, PolicyError> {
        let text = std::fs::read_to_string(path).map_err(|source| PolicyError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml(&text).map_err(|source| PolicyError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        let file: WorkerFile = toml::from_str(text)?;
        Ok(file.worker.unwrap_or_default())
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn a_file_without_a_worker_block_is_the_host_and_not_an_error() {
        // Which is every `luu.toml` written before this existed.
        let config = WorkerConfig::from_toml("[sandbox]\nnetwork = false\n").unwrap();
        assert_eq!(config, WorkerConfig::default());
        assert_eq!(config.runtime, Runtime::Host);
    }

    #[test]
    fn a_worker_block_reads_as_it_looks() {
        let config =
            WorkerConfig::from_toml("[worker]\nruntime = \"docker\"\nimage = \"luu-worker:dev\"\n")
                .unwrap();
        assert_eq!(config.runtime, Runtime::Docker);
        assert_eq!(config.image.as_deref(), Some("luu-worker:dev"));
    }

    #[test]
    fn a_worker_block_from_before_the_seam_had_a_clock_gets_the_default_one() {
        // The field's absence must not read as zero: a zero patience is a seam
        // that kills every worker on its first call, which is worse than the
        // hang it replaced.
        let config = WorkerConfig::from_toml("[worker]\nruntime = \"direct\"\n").unwrap();
        assert_eq!(config.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(
            WorkerConfig::from_toml("[worker]\ntimeout-ms = 90000\n")
                .unwrap()
                .timeout_ms,
            90_000,
        );
        assert_eq!(
            WorkerSpec::new(Runtime::Direct, "/tmp").patience(),
            Duration::from_millis(DEFAULT_TIMEOUT_MS),
        );
    }

    #[test]
    fn a_misspelled_key_is_an_error_like_every_other_grant() {
        // `runtimee = "docker"` that parsed and did nothing would read as a
        // session whose tools run in a container while they ran here.
        assert!(WorkerConfig::from_toml("[worker]\nruntimee = \"docker\"\n").is_err());
        assert!(WorkerConfig::from_toml("[worker]\nruntime = \"lxc\"\n").is_err());
    }
}
