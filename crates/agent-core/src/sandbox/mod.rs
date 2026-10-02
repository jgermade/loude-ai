//! What the agent may reach, and who is holding it to that.
//!
//! Three rungs, of which this module builds the first two
//! (`RECORD/2026-08-27.tools-and-sandbox.completed.md`):
//!
//! 1. **In-process checks.** Canonicalize, compare against the policy, refuse.
//!    Everything an in-process tool can have, and nothing a subprocess gets:
//!    the check happens before the syscall, in a program that then makes the
//!    syscall itself. A child makes its own.
//! 2. **The kernel, same process tree, no image and no daemon.** Landlock for
//!    the filesystem and seccomp for sockets, applied to the child between
//!    `fork` and `exec`. Linux-only; see [`Enforcement`] for what happens
//!    elsewhere.
//! 3. **A container.** Later, and on top of this rather than instead of it —
//!    Landlock survives `exec` and cannot be dropped.
//!
//! Nothing here may say "sandboxed" without saying by what. Every verdict
//! carries [`Applied`], because a run whose subprocesses the kernel held and a
//! run whose subprocesses nothing held are not the same run, and afterwards the
//! recording is the only thing that could tell them apart.

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use crate::job::JobId;

use serde::{Deserialize, Serialize};

pub mod beneath;
pub mod policy;
pub mod proxy;

pub use policy::{Access, Enforcement, Limits, PathRule, PolicyError, SandboxPolicy};
pub use proxy::{EgressFilter, EgressProxy};

#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(not(target_os = "linux"), path = "fallback.rs")]
mod kernel;

/// Read and execute, never write, granted **to a subprocess** whenever any
/// command is allowed — a program cannot run without reading its own
/// interpreter and libc.
///
/// Subprocess only, and that is the whole point of the distinction: the
/// justification is "a command cannot read libc", which says nothing about
/// `read_file`. Allowing `cargo` must not quietly let the agent read `/etc`
/// itself, so [`Sandbox::check_path`] ignores these and only the Landlock
/// ruleset handed to the child carries them.
///
/// Fixed, in the code, and listed by `luu tools`, so it is a grant a reader can
/// see rather than one they have to infer. Everything else a command needs is
/// the user's to name: `cargo` wants `~/.cargo`, and having to write that line
/// is the design's one informed approval.
pub const SYSTEM_ROOTS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt"];

/// The null device, granted **read-write to a subprocess** on the same terms
/// as [`SYSTEM_ROOTS`] — and separately from them because it is the one path
/// here that execute permission would not answer.
///
/// `git` is what found it: every `git` invocation, `git --version` included,
/// exited 128 in one millisecond with `fatal: could not open '/dev/null' for
/// reading and writing: Permission denied`. Git calls `sanitize_stdfds()`
/// before it does anything else, and that opens `/dev/null` to guarantee fds
/// 0/1/2 exist. A policy that names `git` in `commands` was granting a command
/// that could not start. See
/// `RECORD/2026-09-15.git-could-not-open-dev-null.completed.md`.
///
/// Read-write and still not a hole: writes are discarded and reads are EOF, by
/// definition of the device. Nothing can be persisted through it, hidden in it
/// or read back out of it, which is why every sandbox that runs ordinary
/// programs — bubblewrap, Docker, systemd — mounts it unconditionally.
pub const NULL_DEVICE: &str = "/dev/null";

/// Who held a call, as opposed to who was asked to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Applied {
    /// Our own check, before the fact. All an in-process tool can have.
    Process,
    /// The kernel is holding it. `how` names the mechanism *and its version*:
    /// Landlock's older ABIs cover less — v1 does not restrict `truncate`, v5
    /// does not restrict `ioctl` on devices — and months later nothing else
    /// would say which one ran.
    Kernel { how: String },
    /// Some of it, and `missing` is the whole point of the variant: it is the
    /// difference between this run and a held one. Only reachable under
    /// [`Enforcement::BestEffort`].
    Partial { how: String, missing: String },
}

impl std::fmt::Display for Applied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process => f.write_str("in-process check"),
            Self::Kernel { how } => write!(f, "{how}"),
            Self::Partial { how, missing } => write!(f, "{how} (missing: {missing})"),
        }
    }
}

/// What the sandbox decided, and who is enforcing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub allowed: bool,
    /// The rule that decided, as a reader would name it. Present on both
    /// answers: "denied" without which rule denied is a bug report nobody can
    /// act on, and "allowed" without which rule allowed hides the grant that
    /// was wider than someone thought.
    pub rule: String,
    pub enforced_by: Applied,
}

impl Verdict {
    pub fn allow(rule: impl Into<String>, enforced_by: Applied) -> Self {
        Self {
            allowed: true,
            rule: rule.into(),
            enforced_by,
        }
    }

    /// The same verdict, said again once the open has happened and something
    /// better than our own check turned out to be holding it.
    ///
    /// A verdict is written *before* the call it describes, so who enforced it
    /// is a guess until the call is made. On Linux the guess is upgraded here
    /// from `in-process check` to the syscall that actually refused to leave
    /// the tree.
    pub fn held_by(mut self, enforced_by: Applied) -> Self {
        self.enforced_by = enforced_by;
        self
    }

    pub fn deny(rule: impl Into<String>) -> Self {
        Self {
            allowed: false,
            rule: rule.into(),
            enforced_by: Applied::Process,
        }
    }
}

/// One granted tree, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// Canonical, so a symlink cannot walk out of it.
    pub path: PathBuf,
    pub access: Access,
    /// As it was written, so a verdict can name the rule the user typed rather
    /// than the path it turned into.
    pub source: String,
    /// True for [`SYSTEM_ROOTS`], which the sandbox adds itself.
    pub implicit: bool,
}

impl std::fmt::Display for Root {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let implicit = match self.implicit {
            true => ", implicit",
            false => "",
        };
        write!(f, "{} ({}{implicit})", self.source, self.access)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("{source_path}: a granted path has to exist ({source})")]
    MissingPath {
        source_path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the working directory could not be resolved: {0}")]
    Base(#[source] std::io::Error),
}

/// Who granted what this sandbox holds.
///
/// It rides on every denial for the same reason [`Applied`] rides on every
/// allow: a run refused by the policy file and a run refused by the plan its
/// task was approved with are not the same run, and afterwards the recording is
/// the only thing that could tell them apart.
///
/// Serialized because it crosses the worker IPC: a denial that reached the host
/// having lost which authority refused would read as a policy refusal whatever
/// it was, and those are different runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Adjacently tagged, not internally: `Plan(JobId)` is a newtype variant
// wrapping a number, and an internal tag has nowhere to put it.
#[serde(tag = "granted_by", content = "job", rename_all = "snake_case")]
pub enum Authority {
    /// The policy file plus whatever the flags added to it.
    Policy,
    /// The plan a person approved, which narrows the policy for one job.
    Plan(JobId),
    /// The floor a turn nobody approved runs on: the policy file with every
    /// grant downgraded to a read. See [`Sandbox::read_only`] and
    /// `RECORD/2026-09-21.the-drafts-floor.completed.md`.
    ///
    /// **`None` is the first prompt of a session and nothing else.** The floor
    /// is derived from the session and not from the job, and a draft's id does
    /// not exist when the sandbox is chosen — `Context::open_draft` runs when a
    /// turn lands, so on the *first* prompt there is no draft to name. Every
    /// turn after it has one open, sitting in the context the caller already
    /// reads, and `serve` stamps it here so a refusal denormalises its job the
    /// way [`Authority::Plan`] always has.
    ///
    /// It is stamped only when the live job really is a draft: a job with a
    /// plan reaching the floor is the widening
    /// `RECORD/2026-09-21.the-drafts-floor.completed.md` §The second finding
    /// closed, and naming it a draft would be a recording agreeing with a
    /// panel about something false. See
    /// `RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`.
    Draft(Option<JobId>),
    /// A person, for one call a draft's floor refused: *allow once*. The
    /// policy file's sandbox runs it, and this is what that run is stamped
    /// with, so a recording does not say the policy file allowed a write in a
    /// draft. Carries the draft the way [`Authority::Draft`] does, and `None`
    /// for the same reason. See
    /// `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
    Person(Option<JobId>),
}

impl std::fmt::Display for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy => write!(f, "the sandbox policy"),
            Self::Plan(job) => write!(f, "the approved plan for job {job}"),
            Self::Draft(Some(job)) => {
                write!(f, "the floor for draft {job}, which grants no writes")
            }
            Self::Draft(None) => write!(f, "the draft's floor, which grants no writes"),
            Self::Person(Some(job)) => write!(f, "a person, for this call only, in draft {job}"),
            Self::Person(None) => write!(f, "a person, for this call only"),
        }
    }
}

/// What a draft or a plan tells the model about itself, as opposed to what
/// this module enforces without saying so. `Authority::Policy` has no note —
/// it is the ordinary grant, and nothing here has anything to tell a model
/// that its own tools would not already show it. See
/// `RECORD/2026-09-22.an-authority-a-model-is-told.completed.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorityNote {
    /// The words a caller chose to send. Never invented here — this module
    /// enforces the authority and a caller decides whether, and how, to say
    /// so.
    pub text: String,
    /// Where the caller put it. Recorded beside the words because a reader
    /// comparing two runs needs to know where a note landed, not only what it
    /// said — see [`NotePosition`].
    #[serde(default)]
    pub position: NotePosition,
}

/// Where an authority's note is rendered, once a caller decided to send one.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotePosition {
    /// Appended to the stable prefix — free while the authority holds, since
    /// it never changes a turn already sent under it. The default.
    #[default]
    System,
    /// Prepended to the turn's own prompt, every turn — the one worth having
    /// when the question is whether proximity to the turn the model is about
    /// to make changes anything on its own.
    Prompt,
}

/// A resolved policy, and the thing tools ask.
#[derive(Debug, Clone)]
pub struct Sandbox {
    base: PathBuf,
    roots: Vec<Root>,
    commands: Vec<String>,
    network: bool,
    egress: Vec<String>,
    proxy: Option<String>,
    enforcement: Enforcement,
    /// What a child may spend. Carried beside `roots` because it is the same
    /// kind of thing: part of what the child is held to, decided in the parent.
    limits: Limits,
    authority: Authority,
}

/// The answer to "may I touch this path", with the path as it resolved. The
/// resolved path is on both answers because a denial that does not say what the
/// argument turned into is unreadable the moment a symlink is involved.
#[derive(Debug, Clone)]
pub struct PathCheck {
    pub verdict: Verdict,
    pub path: PathBuf,
    /// The granted tree that answered, when one did.
    ///
    /// It was always known — [`Sandbox::root_for`] picked it — and was thrown
    /// away. [`Sandbox::open_beneath`] needs it, because the kernel's answer to
    /// *is this path inside the sandbox* is spelled "resolve it relative to this
    /// directory and refuse to leave".
    pub root: Option<PathBuf>,
}

impl PathCheck {
    /// The root that granted this path, and the path relative to it — which is
    /// what `openat2` resolves.
    ///
    /// Owned rather than borrowed, because the open happens on a blocking
    /// thread (`RECORD/2026-09-05.a-clock-where-there-is-no-seam.completed.md`)
    /// and a borrow cannot go there.
    pub fn beneath(&self) -> Option<(PathBuf, PathBuf)> {
        let root = self.root.clone()?;
        let relative = self
            .path
            .strip_prefix(&root)
            .unwrap_or(&self.path)
            .to_path_buf();
        if !relative.as_os_str().is_empty() {
            return Some((root, relative));
        }
        // The root asked for by its own name. `.` beneath it resolves only when
        // the root is a *directory*, and a plan that grants a file makes the
        // file the root — the ordinary case at the gate, where a model names
        // the file it wants. `openat2` handed a regular file as its `dirfd`
        // answers ENOTDIR, so between 2026-09-05 and this line an approved plan
        // could not read the one path it had just been approved for.
        //
        // Resolved as its last component instead, beneath the directory holding
        // it. Nothing widens: a root is canonical, so that component is not a
        // symlink, and one component that is not `..` cannot leave the
        // directory it resolves in. See
        // `RECORD/2026-09-08.the-surfaces-first.completed.md`.
        match (root.parent(), root.file_name()) {
            (Some(parent), Some(name)) => Some((parent.to_path_buf(), PathBuf::from(name))),
            // `/`, which has no component to be named by.
            _ => Some((root, PathBuf::from("."))),
        }
    }
}

impl Sandbox {
    /// Resolves a policy against a working directory.
    ///
    /// Every granted path must exist: Landlock takes a file descriptor per
    /// root, so a path that is not there cannot be granted — and a rule that
    /// silently did nothing is worse than one that fails to load.
    pub fn new(policy: &SandboxPolicy, base: &Path) -> Result<Self, SandboxError> {
        let base = base.canonicalize().map_err(SandboxError::Base)?;

        let mut roots = Vec::with_capacity(policy.paths.len() + SYSTEM_ROOTS.len());
        for rule in &policy.paths {
            let source = rule.path.display().to_string();
            let resolved = expand_home(&rule.path);
            let resolved = match resolved.is_absolute() {
                true => resolved,
                false => base.join(resolved),
            };
            let path =
                resolved
                    .canonicalize()
                    .map_err(|source_error| SandboxError::MissingPath {
                        source_path: source.clone(),
                        source: source_error,
                    })?;
            roots.push(Root {
                path,
                access: rule.access,
                source,
                implicit: false,
            });
        }

        // A command that cannot read libc cannot run at all, so allowing any
        // command implies these. Skipped silently when absent: a distribution
        // without /opt is not a misconfiguration.
        if !policy.commands.is_empty() {
            for name in SYSTEM_ROOTS {
                let Ok(path) = Path::new(name).canonicalize() else {
                    continue;
                };
                if roots.iter().any(|root| root.path == path) {
                    continue;
                }
                roots.push(Root {
                    path,
                    access: Access::Execute,
                    source: (*name).to_string(),
                    implicit: true,
                });
            }
            // Read-write rather than execute, which is why it is not in the
            // list above: a program opens the null device, it does not run it.
            if let Ok(path) = Path::new(NULL_DEVICE).canonicalize()
                && !roots.iter().any(|root| root.path == path)
            {
                roots.push(Root {
                    path,
                    access: Access::ReadWrite,
                    source: NULL_DEVICE.to_string(),
                    implicit: true,
                });
            }
        }

        // Longest first, so the most specific rule is the one that answers and
        // a narrower rule under a broader one grants more rather than less.
        roots.sort_by(|a, b| {
            b.path
                .components()
                .count()
                .cmp(&a.path.components().count())
                .then_with(|| a.path.cmp(&b.path))
        });

        Ok(Self {
            base,
            roots,
            commands: policy.commands.clone(),
            network: policy.network,
            egress: policy.egress.clone(),
            proxy: policy.proxy.clone(),
            enforcement: policy.enforcement,
            limits: policy.limits,
            authority: Authority::Policy,
        })
    }

    /// The same policy, resolved as one task's rather than the session's.
    ///
    /// Only the label changes: what a narrowed policy grants is decided by
    /// [`crate::task::Plan::narrow`], which builds it out of what this sandbox
    /// already allows. A plan cannot grant what the file does not.
    pub fn under(mut self, authority: Authority) -> Self {
        self.authority = authority;
        self
    }

    /// The same sandbox with every grant downgraded to a read: the floor a turn
    /// nobody approved runs on.
    ///
    /// **Infallible, and that is why it downgrades in place** rather than going
    /// back through [`SandboxPolicy`] and [`Sandbox::new`] the way
    /// [`crate::job::Plan::narrow`] does. That route re-canonicalises every
    /// path, re-adds the implicit [`SYSTEM_ROOTS`], and can fail — and a floor
    /// that can fail needs a fallback, whose only safe value is *nothing
    /// granted*, which turns one missing path into a session that cannot read
    /// its own tree. Nothing here can fail, so the question does not arise.
    ///
    /// Commands, network, egress, proxy, enforcement and limits are the
    /// session's, unchanged. What a command may *do* is decided by the paths it
    /// may touch, and subtracting the allowlist as well would take `rg` and
    /// `ls` — the two things exploration is for — to restate a bound the roots
    /// already hold. See `RECORD/2026-09-21.the-drafts-floor.completed.md`
    /// §What is rejected.
    ///
    /// **The implicit roots are left alone**, and that is not an optimisation.
    /// [`SYSTEM_ROOTS`] and [`NULL_DEVICE`] are granted so a *subprocess can
    /// start* — they are excluded from [`Sandbox::check_path`], from
    /// [`Sandbox::access_for`] and from [`Sandbox::to_policy`], so they were
    /// never part of what the agent may reach and there is no write bit in them
    /// to take. Downgrading them anyway takes `/dev/null` read-write with it,
    /// and a `git` that cannot open `/dev/null` exits 128 before `main` — see
    /// `RECORD/2026-09-15.git-could-not-open-dev-null.completed.md`, which is
    /// the test that caught this.
    pub fn read_only(&self) -> Self {
        let mut floor = self.clone();
        for root in floor.roots.iter_mut().filter(|root| !root.implicit) {
            root.access = Access::Read;
        }
        floor.authority = Authority::Draft(None);
        floor
    }

    /// Which authority answers here — the policy file, a job's plan, or the
    /// floor a turn nobody approved runs on.
    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    /// The declaration this sandbox would be rebuilt from — the inverse of
    /// [`Sandbox::new`].
    ///
    /// It exists so a resolved sandbox can cross a process boundary: the worker
    /// on the far side of the IPC cannot be handed *this* type, because every
    /// path in it was resolved against a filesystem that is not the worker's.
    /// A policy is portable; a resolved sandbox is not. See
    /// `RECORD/2026-09-02.the-worker-and-the-seam.completed.md`.
    ///
    /// Implicit roots are left out, because the far side adds its **own** — the
    /// `/usr` a child needs to reach its libc is the image's, not ours. That is
    /// the same reason they are left out of [`Sandbox::access_for`].
    ///
    /// What does not survive: `Root::source`, the path as the user typed it. A
    /// rebuilt sandbox grants exactly what this one grants and names it by its
    /// canonical path, so a verdict reads `/home/you/.cargo` where this one
    /// would have read `~/.cargo`.
    pub fn to_policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            paths: self
                .roots
                .iter()
                .filter(|root| !root.implicit)
                .map(|root| PathRule::new(root.path.clone(), root.access))
                .collect(),
            commands: self.commands.clone(),
            network: self.network,
            egress: self.egress.clone(),
            proxy: self.proxy.clone(),
            enforcement: self.enforcement,
            limits: self.limits,
        }
    }

    /// What this sandbox grants on a path, if anything. `None` is "no rule
    /// covers it", which is the same answer [`Sandbox::check_path`] denies on.
    ///
    /// Implicit roots are excluded for the same reason they are excluded from
    /// `check_path`: the grant that exists so a child can read libc is not a
    /// grant a plan may inherit.
    pub fn access_for(&self, path: &Path) -> Option<Access> {
        let resolved = resolve(&self.base, path);
        self.root_for(&resolved, false).map(|root| root.access)
    }

    /// The working directory relative paths are resolved against.
    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    pub fn commands(&self) -> &[String] {
        &self.commands
    }

    pub fn network(&self) -> bool {
        self.network
    }

    pub fn egress(&self) -> &[String] {
        &self.egress
    }

    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    pub fn with_proxy(mut self, proxy: Option<String>) -> Self {
        self.proxy = proxy;
        self
    }

    pub fn allows_domain(&self, domain: &str) -> bool {
        if !self.network {
            return false;
        }
        if self.egress.is_empty() {
            return true;
        }
        EgressFilter::new(self.egress.clone()).is_allowed(domain)
    }

    pub fn enforcement(&self) -> Enforcement {
        self.enforcement
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// May a tool touch this path, and where does it actually point?
    ///
    /// The path is canonicalized before it is compared, or a symlink walks
    /// straight out of the sandbox. What canonicalization cannot close is the
    /// race between this answer and the `open` that follows it — that is level
    /// one's hole, the answer for subprocesses is level two, and the answer for
    /// in-process tools is `openat2(RESOLVE_BENEATH)`, which is not in this
    /// change.
    pub fn check_path(&self, path: &Path, needed: Access) -> PathCheck {
        self.check(path, needed, false)
    }

    /// The same, for the program a subprocess is about to become.
    ///
    /// This one *does* consult [`SYSTEM_ROOTS`], because the child will be
    /// running under a ruleset that includes them. Separate from
    /// [`Sandbox::check_path`] so that the grant which exists for the child
    /// cannot leak into a tool that reads files in our own process.
    pub fn check_program(&self, path: &Path) -> PathCheck {
        self.check(path, Access::Execute, true)
    }

    fn check(&self, path: &Path, needed: Access, implicit: bool) -> PathCheck {
        let resolved = resolve(&self.base, path);
        let matched = self.root_for(&resolved, implicit);
        let root = matched
            .filter(|root| root.access >= needed)
            .map(|root| root.path.clone());
        let verdict = match matched {
            Some(root) if root.access >= needed => {
                Verdict::allow(root.to_string(), Applied::Process)
            }
            Some(root) => Verdict::deny(format!(
                "{root} does not grant {needed} on {}, under {}",
                resolved.display(),
                self.authority,
            )),
            None => Verdict::deny(format!(
                "no rule grants {needed} on {}, under {}",
                resolved.display(),
                self.authority,
            )),
        };
        PathCheck {
            verdict,
            path: resolved,
            root,
        }
    }

    /// Opens a checked path in a way the kernel keeps inside the tree that
    /// granted it.
    ///
    /// The check above answers *may this be opened*; this answers it again,
    /// atomically with the open, because between the two the path is a string
    /// and a string is not a file. On Linux that is `openat2(RESOLVE_BENEATH)`
    /// and the verdict it returns says the kernel held it; everywhere else it
    /// is the same open by path it always was, and the verdict says that
    /// instead. See `RECORD/2026-09-05.beneath-the-root.completed.md`.
    ///
    /// `Err` is an ordinary failed call — a missing file, or the resolution
    /// rules refusing, which [`beneath::is_escape`] tells apart so a denial does
    /// not reach a model dressed as a typo.
    pub fn open_beneath(
        &self,
        check: &PathCheck,
        mode: beneath::Mode,
    ) -> std::io::Result<beneath::Opened> {
        let Some((root, relative)) = check.beneath() else {
            // Nothing granted it, so there is nothing to resolve beneath. A
            // caller that reaches here skipped the verdict, which is a bug
            // rather than a denial to report.
            return Err(std::io::Error::other(
                "open_beneath on a path no rule granted",
            ));
        };
        beneath::open(&root, &relative, mode)
    }

    /// The most specific rule covering a resolved path, if any. `roots` is
    /// sorted longest first, so the first hit is it.
    fn root_for(&self, path: &Path, implicit: bool) -> Option<&Root> {
        self.roots
            .iter()
            .filter(|root| implicit || !root.implicit)
            .find(|root| path.starts_with(&root.path))
    }

    /// Checks a program name against the allowlist and prepares what the kernel
    /// will hold the child to.
    ///
    /// `Err` is the verdict that refused, which is also what the caller reports:
    /// a denial here is an ordinary outcome of a tool call, not a failure of the
    /// program.
    pub fn prepare_command(&self, program: &str) -> Result<Restrictions, Verdict> {
        if !self.commands.iter().any(|allowed| allowed == program) {
            return Err(Verdict::deny(match self.commands.is_empty() {
                true => format!(
                    "no commands are allowed by {}: `{program}` needs a `commands` entry",
                    self.authority,
                ),
                false => format!(
                    "`{program}` is not in commands = [{}], under {}",
                    self.commands.join(", "),
                    self.authority,
                ),
            }));
        }

        let prepared = kernel::prepare(&self.roots, self.network);
        // The rlimits join `how` rather than living beside it: they are one
        // more mechanism holding this child, and the rule is that a verdict
        // names every one of them. On macOS this is the whole of `how` — the
        // first thing that has ever actually held a child there.
        let how = match (prepared.how.clone(), self.limits.describe()) {
            (Some(kernel), Some(limits)) => Some(format!("{kernel} + {limits}")),
            (kernel, limits) => kernel.or(limits),
        };
        let enforced_by = match (&how, &prepared.missing) {
            (Some(how), None) => Applied::Kernel { how: how.clone() },
            (Some(how), Some(missing)) => Applied::Partial {
                how: how.clone(),
                missing: missing.clone(),
            },
            (None, missing) => Applied::Partial {
                how: "in-process check only".into(),
                missing: missing.clone().unwrap_or_else(|| "everything".into()),
            },
        };

        // The one place a security property is a setting. Under `kernel` a gap
        // is a denial that names what is missing; under `best-effort` it is a
        // verdict that carries it.
        if self.enforcement == Enforcement::Kernel
            && let Applied::Partial { missing, .. } = &enforced_by
        {
            // Which authority is holding this child is now a real question: a
            // plan may tighten enforcement, so the answer is no longer always
            // the policy file, and a denial that named the wrong one sends a
            // person to edit the wrong thing. What it cannot say is which of
            // the two *set* the value — see
            // `RECORD/2026-09-06.enforcement-per-job.completed.md`.
            return Err(Verdict::deny(match self.authority {
                // A person allowing one call widens it to the policy file, and
                // no further: what the policy cannot hold, a click cannot.
                Authority::Policy | Authority::Person(_) => format!(
                    "the kernel cannot hold this child ({missing}); \
                     grant it anyway with enforcement = \"best-effort\""
                ),
                Authority::Plan(job) => format!(
                    "the kernel cannot hold this child ({missing}); \
                     job {job} runs under kernel enforcement — loosen it in the \
                     plan if the policy allows it, in the policy otherwise"
                ),
                // The floor is a subtraction, so a gap in it is the one case
                // where "grant it anyway" is the wrong advice: what is missing
                // is not a grant but the thing holding the child *to* reads.
                // Approving a plan is the way forward, and it is also the way
                // the loosening becomes somebody's decision.
                Authority::Draft(_) => format!(
                    "the kernel cannot hold this child ({missing}); \
                     a draft runs on the floor, which grants no writes — \
                     approve a plan to run this"
                ),
            }));
        }

        Ok(Restrictions {
            verdict: Verdict::allow(format!("commands allows `{program}`"), enforced_by),
            prepared,
            limits: self.limits,
        })
    }
}

/// The kernel restrictions for one child, and the verdict that goes with them.
pub struct Restrictions {
    pub verdict: Verdict,
    prepared: kernel::Prepared,
    limits: Limits,
}

impl std::fmt::Debug for Restrictions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The prepared side is a file descriptor and a BPF program; the verdict
        // is the part anyone reading a failure wants.
        f.debug_struct("Restrictions")
            .field("verdict", &self.verdict)
            .finish_non_exhaustive()
    }
}

impl Restrictions {
    /// Arranges for the restrictions to be applied in the child, after `fork`
    /// and before `exec`.
    ///
    /// The limits go on first, and the order is not cosmetic: `setrlimit` is
    /// itself a syscall, and installing a filter before it would make the
    /// sandbox's own setup subject to the sandbox.
    pub fn install(self, command: &mut std::process::Command) {
        #[cfg(unix)]
        install_limits(self.limits, command);
        self.prepared.install(command);
    }
}

/// `setrlimit`'s first argument, which glibc types differently from everyone
/// else — the only reason this alias exists.
#[cfg(all(unix, target_env = "gnu"))]
type Resource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(target_env = "gnu")))]
type Resource = libc::c_int;

/// The limits, applied in the child between `fork` and `exec`.
///
/// This lives here rather than in `kernel` because `setrlimit` is POSIX: the
/// `linux.rs`/`fallback.rs` split is about what only Linux has, and a limit
/// both platforms honour would be duplicated on both sides of it.
#[cfg(unix)]
fn install_limits(limits: Limits, command: &mut std::process::Command) {
    if limits.is_empty() {
        return;
    }
    const MIB: u64 = 1024 * 1024;
    let cpu = limits.cpu_seconds;
    let file = limits.file_size_mb.map(|mb| mb.saturating_mul(MIB));
    let memory = limits.memory_mb.map(|mb| mb.saturating_mul(MIB));
    let processes = limits.processes;

    // SAFETY: the closure runs after `fork` and before `exec`, where only
    // async-signal-safe work is allowed. It allocates nothing and makes one
    // `setrlimit` syscall per limit; `last_os_error` stores a raw errno without
    // touching the allocator.
    unsafe {
        command.pre_exec(move || {
            set_cpu_limit(cpu)?;
            set_limit(libc::RLIMIT_FSIZE, file)?;
            set_limit(libc::RLIMIT_AS, memory)?;
            set_limit(libc::RLIMIT_NPROC, processes)?;
            Ok(())
        });
    }
}

/// The CPU limit, and the one place soft and hard are deliberately *not* equal.
///
/// `RLIMIT_CPU` is two-stage by design: the soft limit sends `SIGXCPU`, and the
/// hard one sends `SIGKILL`. Setting them equal collapses that into a plain
/// `SIGKILL` — measured, not assumed: the first version of this did, and the
/// child came back as signal 9, indistinguishable from a crash or an OOM kill.
/// One second of grace buys the signal that *names the limit*, which is what
/// `run_command` reports and what a judge would read. The cap it widens is one
/// second, and the hard limit still ends the child if `SIGXCPU` is caught and
/// ignored.
#[cfg(unix)]
fn set_cpu_limit(seconds: Option<u64>) -> std::io::Result<()> {
    let Some(seconds) = seconds else {
        return Ok(());
    };
    let limit = libc::rlimit {
        rlim_cur: seconds as libc::rlim_t,
        rlim_max: seconds.saturating_add(1) as libc::rlim_t,
    };
    // SAFETY: `limit` is a valid, initialised `rlimit` for the whole call.
    match unsafe { libc::setrlimit(libc::RLIMIT_CPU, &limit) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// One limit, soft and hard together — a child that could raise its own soft
/// limit back to the hard one is not limited. [`set_cpu_limit`] is the
/// exception, and says why.
#[cfg(unix)]
fn set_limit(resource: Resource, value: Option<u64>) -> std::io::Result<()> {
    let Some(value) = value else { return Ok(()) };
    let limit = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: `limit` is a valid, initialised `rlimit` for the whole call.
    match unsafe { libc::setrlimit(resource, &limit) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// `~` and `~/…`, against `$HOME`. Left alone when there is no `$HOME`, so the
/// path fails to resolve with its own name in the error rather than silently
/// meaning something else.
fn expand_home(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => path.to_path_buf(),
    }
}

/// An absolute, symlink-free path, for something that may not exist yet.
///
/// Canonicalizes the deepest existing ancestor and re-attaches the rest. The
/// re-attached components are by definition not there, so none of them can be a
/// symlink and resolving `..` textually across them is exact — which is what
/// makes `proj/nothing/../../etc/passwd` land on `/etc/passwd` rather than
/// inside the project.
fn resolve(base: &Path, path: &Path) -> PathBuf {
    let joined = match path.is_absolute() {
        true => path.to_path_buf(),
        false => base.join(path),
    };

    // Popped as components rather than with `file_name`, which is `None` for a
    // path ending in `..` — and stopping there would leave the `..` in the
    // result, where `starts_with` reads it as an ordinary directory name and
    // `proj/nothing/../../outside` compares as if it were inside `proj`.
    let mut prefix: Vec<std::ffi::OsString> = joined
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect();
    let mut tail = Vec::new();
    while !prefix.is_empty() {
        if prefix.iter().collect::<PathBuf>().exists() {
            break;
        }
        tail.push(prefix.pop().expect("checked non-empty"));
    }

    let existing: PathBuf = prefix.iter().collect();
    let mut out = existing.canonicalize().unwrap_or(existing);
    for component in tail.iter().rev() {
        match component.to_str() {
            Some("..") => {
                out.pop();
            }
            Some(".") => {}
            _ => out.push(component),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway tree with a project directory and a secret outside it.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "luu-sandbox-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("proj/src")).unwrap();
            std::fs::create_dir_all(root.join("outside")).unwrap();
            std::fs::write(root.join("proj/src/main.rs"), "fn main() {}").unwrap();
            std::fs::write(root.join("outside/secret"), "shh").unwrap();
            Self { root }
        }

        fn proj(&self) -> PathBuf {
            self.root.join("proj").canonicalize().unwrap()
        }

        fn sandbox(&self, policy: &SandboxPolicy) -> Sandbox {
            Sandbox::new(policy, &self.proj()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_sandbox_survives_being_rebuilt_from_its_own_declaration() {
        // The property the worker IPC rests on: what crosses the pipe is the
        // policy, and the far side resolves it. If this round trip lost a
        // grant, a contained run would be silently narrower than a host one;
        // if it gained one, the container would be the wider sandbox.
        let fixture = Fixture::new("to-policy");
        let mut policy = read_write_here();
        policy.commands = vec!["ls".into()];
        policy.allow(fixture.root.join("outside"), Access::Read);
        let original = fixture.sandbox(&policy);

        let rebuilt = Sandbox::new(&original.to_policy(), original.base())
            .unwrap()
            .under(Authority::Plan(7));

        for path in [
            fixture.proj().join("src/main.rs"),
            fixture.root.join("outside/secret"),
            fixture.root.join("nowhere"),
        ] {
            for needed in [Access::Read, Access::ReadWrite] {
                assert_eq!(
                    original.check_path(&path, needed).verdict.allowed,
                    rebuilt.check_path(&path, needed).verdict.allowed,
                    "{} at {needed}",
                    path.display(),
                );
            }
        }
        assert_eq!(original.commands(), rebuilt.commands());
        assert_eq!(original.network(), rebuilt.network());
        assert_eq!(original.enforcement(), rebuilt.enforcement());
        assert_eq!(original.limits(), rebuilt.limits());
        // The authority is the caller's to re-attach, and a denial from the far
        // side has to name the plan that refused rather than the policy file.
        assert!(
            rebuilt
                .check_path(&fixture.root.join("nowhere"), Access::Read)
                .verdict
                .rule
                .contains("the approved plan for job 7")
        );
        // The implicit roots are *not* carried: the far side adds its own,
        // because the `/usr` a child needs is the one it is standing on.
        assert!(
            original
                .to_policy()
                .paths
                .iter()
                .all(|rule| { !SYSTEM_ROOTS.contains(&rule.path.to_str().unwrap_or_default()) }),
        );
    }

    #[test]
    fn the_authority_a_denial_names_survives_a_json_round_trip() {
        // Adjacently tagged, because `Plan(TaskId)` is a newtype variant and an
        // internal tag has nowhere to put the number — which serde discovers at
        // runtime rather than at compile time, so it is worth a test.
        // Both `Draft` arms: the stamped one is a second newtype variant on the
        // same adjacent tag, and the unstamped one puts `null` in the content
        // field — which is the case a `skip_serializing_if` would have quietly
        // turned into a missing key.
        for authority in [
            Authority::Policy,
            Authority::Plan(12),
            Authority::Draft(None),
            Authority::Draft(Some(3)),
            Authority::Person(None),
            Authority::Person(Some(3)),
        ] {
            let text = serde_json::to_string(&authority).unwrap();
            assert_eq!(
                serde_json::from_str::<Authority>(&text).unwrap(),
                authority,
                "{text}"
            );
        }
    }

    /// The floor: every grant downgraded to a read, and nothing else moved.
    ///
    /// The second assertion is the one worth having. A floor that also dropped
    /// the commands would pass a test that only checked the write bit, and it
    /// would take `rg` and `ls` — the two things exploration is for — with it.
    #[test]
    fn the_floor_subtracts_the_write_bit_and_nothing_else() {
        let mut policy = read_write_here();
        policy.allow_command("rg");
        policy.network = true;
        let session = Sandbox::new(&policy, &std::env::current_dir().unwrap()).unwrap();
        assert!(
            session
                .check_path(std::path::Path::new("Cargo.toml"), Access::ReadWrite)
                .verdict
                .allowed,
            "the session has to grant the write, or the floor subtracts nothing",
        );

        let floor = session.read_only();
        assert!(
            !floor
                .check_path(std::path::Path::new("Cargo.toml"), Access::ReadWrite)
                .verdict
                .allowed,
        );
        assert!(
            floor
                .check_path(std::path::Path::new("Cargo.toml"), Access::Read)
                .verdict
                .allowed,
            "a floor is a floor and not a wall",
        );
        assert_eq!(floor.commands(), session.commands());
        assert_eq!(floor.network(), session.network());
        assert_eq!(floor.egress(), session.egress());
        assert_eq!(floor.enforcement(), session.enforcement());
        assert_eq!(floor.base(), session.base());
        // Unstamped: `read_only` is the session's floor and knows no job. What
        // names a draft is `serve`, one turn in — see
        // `RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`.
        assert_eq!(floor.authority(), &Authority::Draft(None));
        assert_eq!(
            floor.roots().len(),
            session.roots().len(),
            "downgrading in place must not add or drop a root — which going back \
             through `Sandbox::new` would, by re-adding the implicit ones",
        );

        // And the one the first version of this got wrong. The implicit grants
        // exist so a subprocess can *start*, not so the agent can reach
        // anything, and `/dev/null` is the one of them that is read-write: a
        // floor that takes it leaves a `git` that exits 128 before `main`. See
        // `RECORD/2026-09-15.git-could-not-open-dev-null.completed.md`.
        let null_device = floor
            .roots()
            .iter()
            .find(|root| root.path == std::path::Path::new(NULL_DEVICE));
        assert_eq!(
            null_device.map(|root| root.access),
            Some(Access::ReadWrite),
            "the floor took the null device with it, so no command can start",
        );
        assert!(
            floor
                .roots()
                .iter()
                .filter(|root| root.implicit)
                .eq(session.roots().iter().filter(|root| root.implicit)),
            "the floor subtracts what the policy file granted and nothing the \
             sandbox adds for a child to be able to run at all",
        );
    }

    fn read_write_here() -> SandboxPolicy {
        SandboxPolicy {
            paths: vec![PathRule::new(".", Access::ReadWrite)],
            ..SandboxPolicy::default()
        }
    }

    /// A policy that allows `/bin/sh` and holds it to one CPU second.
    #[cfg(unix)]
    fn one_cpu_second() -> SandboxPolicy {
        SandboxPolicy {
            paths: vec![PathRule::new(".", Access::ReadWrite)],
            commands: vec!["/bin/sh".into()],
            network: false,
            // Not because the limits need it: on a kernel without Landlock the
            // gap is a denial under `kernel`, and this test is about the limit
            // rather than about which kernel the runner has.
            enforcement: Enforcement::BestEffort,
            limits: Limits {
                cpu_seconds: Some(1),
                ..Limits::NONE
            },
            ..SandboxPolicy::default()
        }
    }

    /// The limits are in the verdict, with their numbers.
    ///
    /// "rlimits" without them is the same claim for a 300-second limit and a
    /// 1-second one, and afterwards the recording is the only thing that could
    /// tell those runs apart.
    #[cfg(unix)]
    #[test]
    fn a_verdict_names_the_limits_the_child_is_held_to() {
        let fixture = Fixture::new("limits-verdict");
        let sandbox = fixture.sandbox(&one_cpu_second());
        let restrictions = sandbox.prepare_command("/bin/sh").expect("sh is allowed");
        let named = restrictions.verdict.enforced_by.to_string();
        assert!(named.contains("rlimits (cpu 1s)"), "{named}");
    }

    /// And they are not only in the string: the child is actually held.
    ///
    /// A spin loop is what the 30-second clock in `run_command` cannot answer
    /// — it kills what the tool is still waiting for, and this holds what
    /// outlives it. One CPU second, so the test costs one.
    #[cfg(unix)]
    #[test]
    fn a_child_that_spins_forever_is_killed_by_its_cpu_limit() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        let fixture = Fixture::new("limits-cpu");
        let sandbox = fixture.sandbox(&one_cpu_second());
        let restrictions = sandbox.prepare_command("/bin/sh").expect("sh is allowed");

        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("while :; do :; done")
            .current_dir(sandbox.base())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        restrictions.install(&mut command);

        let status = command.status().expect("the child ran");
        assert!(
            !status.success(),
            "the child outlived a limit that was supposed to hold it",
        );
    }

    #[test]
    fn a_path_inside_a_granted_root_is_allowed_and_names_the_rule() {
        let fixture = Fixture::new("inside");
        let sandbox = fixture.sandbox(&read_write_here());

        let check = sandbox.check_path(Path::new("src/main.rs"), Access::ReadWrite);
        assert!(check.verdict.allowed, "{:?}", check.verdict);
        assert!(check.verdict.rule.contains('.'), "{:?}", check.verdict);
        assert_eq!(check.path, fixture.proj().join("src/main.rs"));
    }

    #[test]
    fn dot_dot_does_not_walk_out_even_through_a_directory_that_is_not_there() {
        let fixture = Fixture::new("dotdot");
        let sandbox = fixture.sandbox(&read_write_here());

        for attempt in ["../outside/secret", "nothing/../../outside/secret"] {
            let check = sandbox.check_path(Path::new(attempt), Access::Read);
            assert!(!check.verdict.allowed, "{attempt} was allowed");
            assert!(check.verdict.rule.contains("no rule grants"));
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_is_judged_by_where_it_points() {
        let fixture = Fixture::new("symlink");
        // The classic bypass: a link inside the project to a file outside it.
        std::os::unix::fs::symlink(
            fixture.root.join("outside/secret"),
            fixture.proj().join("src/link"),
        )
        .unwrap();
        let sandbox = fixture.sandbox(&read_write_here());

        let check = sandbox.check_path(Path::new("src/link"), Access::Read);
        assert!(
            !check.verdict.allowed,
            "canonicalizing before comparing is the whole point: {:?}",
            check.verdict
        );
        assert!(check.path.ends_with("outside/secret"));
    }

    #[test]
    fn the_longest_matching_rule_answers_so_a_narrow_grant_can_widen_a_broad_one() {
        let fixture = Fixture::new("longest");
        let sandbox = fixture.sandbox(&SandboxPolicy {
            paths: vec![
                PathRule::new(".", Access::Read),
                PathRule::new("./src", Access::ReadWrite),
            ],
            ..SandboxPolicy::default()
        });

        assert!(
            sandbox
                .check_path(Path::new("src/main.rs"), Access::ReadWrite)
                .verdict
                .allowed
        );
        assert!(
            !sandbox
                .check_path(Path::new("Cargo.toml"), Access::ReadWrite)
                .verdict
                .allowed,
            "the broader rule still grants only read"
        );
    }

    #[test]
    fn too_little_access_is_denied_by_the_rule_that_granted_the_rest() {
        let fixture = Fixture::new("tooLittle");
        let sandbox = fixture.sandbox(&SandboxPolicy {
            paths: vec![PathRule::new(".", Access::Read)],
            ..SandboxPolicy::default()
        });

        let check = sandbox.check_path(Path::new("src/main.rs"), Access::ReadWrite);
        assert!(!check.verdict.allowed);
        assert!(
            check.verdict.rule.contains("does not grant read-write"),
            "{}",
            check.verdict.rule
        );
    }

    #[test]
    fn a_granted_path_that_is_not_there_fails_to_load_rather_than_doing_nothing() {
        let fixture = Fixture::new("missing");
        let error = Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new("./nowhere", Access::Read)],
                ..SandboxPolicy::default()
            },
            &fixture.proj(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("./nowhere"), "{error}");
    }

    #[test]
    fn no_commands_means_no_command() {
        let fixture = Fixture::new("nocmd");
        let sandbox = fixture.sandbox(&read_write_here());

        let verdict = sandbox.prepare_command("ls").unwrap_err();
        assert!(!verdict.allowed);
        assert!(verdict.rule.contains("no commands are allowed"));
        assert!(
            sandbox.roots().iter().all(|root| !root.implicit),
            "the system roots are granted only once a command is"
        );
    }

    #[test]
    fn allowing_a_command_brings_the_system_roots_with_it() {
        let fixture = Fixture::new("syscmd");
        let sandbox = fixture.sandbox(&SandboxPolicy {
            commands: vec!["ls".into()],
            ..read_write_here()
        });

        assert!(
            sandbox
                .roots()
                .iter()
                .any(|root| root.implicit && root.access == Access::Execute),
            "a command that cannot read libc cannot run"
        );
        // Narrowed rather than dropped when `/dev/null` became an implicit
        // read-write grant: what this has always been guarding is that the
        // *system trees* never quietly became writable, and that is still the
        // assertion. The null device is the one exception and is named, so a
        // second one cannot arrive without this line being edited again. See
        // `RECORD/2026-09-15.git-could-not-open-dev-null.completed.md`.
        assert!(
            sandbox.roots().iter().all(|root| {
                !root.implicit
                    || root.access != Access::ReadWrite
                    || root.path == Path::new(NULL_DEVICE)
            }),
            "and nothing but the null device is granted write"
        );
    }

    #[test]
    fn the_null_device_comes_with_a_command_because_git_cannot_start_without_it() {
        let fixture = Fixture::new("devnull");
        let sandbox = fixture.sandbox(&SandboxPolicy {
            commands: vec!["git".into()],
            ..read_write_here()
        });

        assert!(
            sandbox.roots().iter().any(|root| {
                root.implicit
                    && root.path == Path::new(NULL_DEVICE)
                    && root.access == Access::ReadWrite
            }),
            "git opens /dev/null before main() and exits 128 without it"
        );

        // And not for a policy that allows no commands at all: the grant exists
        // for subprocesses, and a session with none has nothing to start.
        let quiet = fixture.sandbox(&read_write_here());
        assert!(
            !quiet.roots().iter().any(|root| root.implicit),
            "no commands, no implicit grants"
        );
    }

    #[test]
    fn allowing_a_command_does_not_let_the_agent_read_the_system_itself() {
        // The implicit roots exist because a child cannot run without reading
        // libc. That justification says nothing about `read_file`, and letting
        // it apply there would mean `commands = ["ls"]` silently granted the
        // agent /etc.
        let fixture = Fixture::new("implicitread");
        let sandbox = fixture.sandbox(&SandboxPolicy {
            commands: vec!["ls".into()],
            ..read_write_here()
        });

        let system = Path::new("/etc");
        assert!(
            !sandbox.check_path(system, Access::Read).verdict.allowed,
            "an in-process tool sees only what was written down",
        );
        assert!(
            sandbox.check_program(system).verdict.allowed,
            "and the child, which will be under a ruleset that includes it, does",
        );
    }

    #[test]
    fn a_command_that_is_not_on_the_list_is_denied_by_name() {
        let fixture = Fixture::new("othercmd");
        let sandbox = fixture.sandbox(&SandboxPolicy {
            commands: vec!["ls".into()],
            ..read_write_here()
        });
        let verdict = sandbox.prepare_command("curl").unwrap_err();
        assert!(
            verdict.rule.contains("`curl` is not in commands"),
            "{verdict:?}"
        );
    }

    #[test]
    fn best_effort_reports_the_gap_rather_than_hiding_it() {
        let fixture = Fixture::new("besteffort");
        let sandbox = fixture.sandbox(&SandboxPolicy {
            commands: vec!["ls".into()],
            enforcement: Enforcement::BestEffort,
            ..read_write_here()
        });

        let verdict = sandbox.prepare_command("ls").unwrap().verdict;
        assert!(verdict.allowed);
        match &verdict.enforced_by {
            // Where the kernel can hold it, the verdict says with what.
            Applied::Kernel { how } => assert!(how.contains("seccomp"), "{how}"),
            // Where it cannot, the verdict says what is missing. Both are
            // correct answers; "sandboxed" with no mechanism named is not.
            Applied::Partial { missing, .. } => assert!(!missing.is_empty()),
            Applied::Process => panic!("a subprocess is never held by an in-process check"),
        }
    }

    #[test]
    fn kernel_enforcement_denies_rather_than_degrading() {
        let fixture = Fixture::new("required");
        let sandbox = fixture.sandbox(&SandboxPolicy {
            commands: vec!["ls".into()],
            enforcement: Enforcement::Kernel,
            ..read_write_here()
        });

        match sandbox.prepare_command("ls") {
            // On a kernel with both mechanisms this is the answer.
            Ok(restrictions) => assert!(matches!(
                restrictions.verdict.enforced_by,
                Applied::Kernel { .. }
            )),
            // On anything else, a denial that says what is missing and how to
            // lower the bar deliberately — never a quiet downgrade.
            Err(verdict) => {
                assert!(!verdict.allowed);
                assert!(verdict.rule.contains("best-effort"), "{verdict:?}");
            }
        }
    }

    #[test]
    fn a_denial_under_a_plan_sends_the_person_to_the_plan() {
        // The case the field exists for: a session that does its best, and one
        // job inside it held to the kernel. Where the kernel cannot hold the
        // child, the denial has to name the job — the session is `best-effort`,
        // so "grant it anyway with enforcement = best-effort" would send a
        // person to change a file that already says that.
        let fixture = Fixture::new("under-a-plan");
        let session = fixture.sandbox(&SandboxPolicy {
            commands: vec!["ls".into()],
            enforcement: Enforcement::BestEffort,
            ..read_write_here()
        });
        let job = Sandbox::new(
            &SandboxPolicy {
                commands: vec!["ls".into()],
                enforcement: Enforcement::Kernel,
                ..read_write_here()
            },
            session.base(),
        )
        .unwrap()
        .under(Authority::Plan(7));

        // The session itself runs it either way; that is what `best-effort` is.
        assert!(session.prepare_command("ls").is_ok());

        match job.prepare_command("ls") {
            // A kernel with both mechanisms holds it, and the plan asked for
            // exactly that.
            Ok(restrictions) => assert!(matches!(
                restrictions.verdict.enforced_by,
                Applied::Kernel { .. }
            )),
            Err(verdict) => {
                assert!(!verdict.allowed);
                assert!(verdict.rule.contains("job 7"), "{verdict:?}");
                assert!(
                    !verdict.rule.contains("grant it anyway"),
                    "the policy file is not where this one is loosened: {verdict:?}",
                );
            }
        }
    }

    #[test]
    fn a_verdict_survives_the_wire() {
        let verdict = Verdict::allow(
            ". (read-write)",
            Applied::Partial {
                how: "seccomp".into(),
                missing: "landlock".into(),
            },
        );
        let json = serde_json::to_value(&verdict).unwrap();
        assert_eq!(json["enforced_by"]["by"], "partial");
        assert_eq!(json["enforced_by"]["missing"], "landlock");
        let back: Verdict = serde_json::from_value(json).unwrap();
        assert_eq!(back, verdict);
    }
}
