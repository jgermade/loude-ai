//! A task: the unit of work, of approval, and of forgetting.
//!
//! A session is a sequence of these rather than a mode the agent is in. One
//! boundary does three jobs — it is the cut point compaction takes, the scope
//! permission is granted at, and the grouping the transcript shows — which is
//! the whole argument for it. See
//! `RECORD/2026-08-27.tasks-instead-of-modes.completed.md` for the argument and
//! `RECORD/2026-08-30.tasks-in-code.completed.md` for what this file does and does not do.
//!
//! **Closing is an event, not a mutation.** Nothing here deletes a turn: a
//! closed task changes how its turns *render* (see [`crate::context`]), so
//! reopening is folding differently rather than undoing a deletion.

use serde::{Deserialize, Serialize};

use crate::context::{Counter, Fragment, TokenCounter};
use crate::sandbox::{Access, Authority, Enforcement, Sandbox, SandboxError, SandboxPolicy};
use crate::tools::{ToolCall, ToolStep};

/// Jobs are numbered per session, in order, starting at 1.
pub type JobId = u64;
pub type TaskId = JobId;

/// How many distinct actions a summary lists before it stops listing them.
///
/// The summary enters the write-once region every later turn is built on, so
/// its size is not free and must not grow with the length of the job. A cap
/// is not a strategy — it is what stops one long job quietly becoming the
/// prompt.
const MAX_EVIDENCE_LINES: usize = 12;

/// How many tokens of quoted source one summary may carry.
///
/// The same argument as [`MAX_EVIDENCE_LINES`], applied to the other half of
/// the summary: a quote is paid for on every call from the close to the end of
/// the session, so what it costs must be a function of this number and not of
/// how much the job was handed. The value is a starting point and wants the
/// grounded probe to choose it — see
/// `RECORD/2026-08-30.what-a-summary-should-carry.completed.md`.
const MAX_QUOTED_TOKENS: u32 = 1024;

/// What the agent proposes to do, and what the user approves.
///
/// Not prose: the files and commands are the part that is checked against the
/// sandbox, so they have to be a list rather than a paragraph someone would
/// have to parse back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The model's checklist of tasks to perform.
    #[serde(default, alias = "steps")]
    pub tasks: Vec<String>,
    /// What the job may **read**.
    #[serde(default)]
    pub files: Vec<String>,
    /// What it may also **change**. A path here need not be repeated in
    /// `files`: writing implies reading, because `edit_file` reads before it
    /// replaces. A plan that declares none may not write at all — the check is
    /// worth having only if the answer can be no.
    #[serde(default)]
    pub writes: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    /// The command line whose success closes this job, rendered exactly as
    /// [`command_line`] renders a call — `"cargo test"`, not `"cargo"`.
    ///
    /// `None` in every plan a model writes, because the model is never asked
    /// for it: this is the one part of a plan that arrives at the gate rather
    /// than from the planning call, and a job whose plan has none closes only
    /// when a person says so. See
    /// `RECORD/2026-09-02.closing-on-an-exit-code.completed.md`.
    #[serde(default)]
    pub closes_on: Option<String>,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub egress: Vec<String>,
    /// How hard the kernel is asked to hold this job's children, when the job
    /// asks at all. `None` — what every plan written before this meant — is the
    /// session's.
    ///
    /// The one field here that narrows *backwards*. Everything else a plan
    /// declares is a grant, where less is narrower and a plan that names
    /// nothing gets nothing; this is a strictness whose permissive value is
    /// [`Enforcement::BestEffort`], so a plan asking for `kernel` is narrowing
    /// and needs no permission, and a plan asking for `best-effort` is asking
    /// the policy file to relax, which [`Plan::unmet`] refuses. See
    /// `RECORD/2026-09-06.enforcement-per-job.completed.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforcement: Option<Enforcement>,
}

impl Plan {
    /// Backwards compatibility accessor for callers that called `steps`.
    pub fn steps(&self) -> &[String] {
        &self.tasks
    }

    /// What the plan asks for that the sandbox does not grant, one line each.
    ///
    /// The design wants the approved plan to *be* the policy for its task, with
    /// the policy file as its floor. This is the half of that which cannot be
    /// wrong: a plan naming a file nobody may touch or a command nobody may run
    /// is refused before anything runs, rather than discovered as a denial four
    /// turns in. Narrowing the sandbox *down* to the plan waits for a human who
    /// can add the file the plan forgot.
    ///
    /// Read and write are asked separately, which is the whole point: a plan
    /// that names a file under a `read` root and then edits it used to pass
    /// here and be denied at the call, four turns in — the one case this check
    /// exists to prevent and the one it missed.
    pub fn unmet(&self, sandbox: &Sandbox) -> Vec<String> {
        let mut unmet = Vec::new();
        for file in &self.files {
            let check = sandbox.check_path(std::path::Path::new(file), Access::Read);
            if !check.verdict.allowed {
                unmet.push(format!("file {file}: {}", check.verdict.rule));
            }
        }
        for file in &self.writes {
            let check = sandbox.check_path(std::path::Path::new(file), Access::ReadWrite);
            if !check.verdict.allowed {
                unmet.push(format!("write {file}: {}", check.verdict.rule));
            }
        }
        for command in &self.commands {
            if !sandbox.commands().iter().any(|allowed| allowed == command) {
                unmet.push(format!(
                    "command {command}: not in the sandbox's allowed commands"
                ));
            }
        }
        // A closing condition the task may not run is one that can never be
        // met, and a task that can never close is worse than one that closes
        // by hand: it looks like it will close itself.
        if let Some(closes_on) = &self.closes_on {
            match closes_on.split_whitespace().next() {
                Some(program) if self.commands.iter().any(|allowed| allowed == program) => {}
                Some(program) => unmet.push(format!(
                    "closes_on {closes_on}: the plan does not allow the command {program}"
                )),
                None => unmet.push("closes_on: empty".to_string()),
            }
        }
        // Backwards from every other check here, and deliberately: the value
        // being refused is the *lenient* one. A plan may tighten this and may
        // not loosen it, or the gate becomes the policy and `luu.toml` is a
        // suggestion.
        if self.enforcement == Some(Enforcement::BestEffort)
            && sandbox.enforcement() == Enforcement::Kernel
        {
            unmet.push(format!(
                "enforcement best-effort: the policy requires kernel enforcement ({})",
                sandbox.authority()
            ));
        }
        if self.network && !sandbox.network() {
            unmet.push(format!(
                "network: not allowed by the policy ({})",
                sandbox.authority()
            ));
        }
        for domain in &self.egress {
            if !sandbox.allows_domain(domain) {
                unmet.push(format!(
                    "egress domain `{domain}`: not allowed by the policy ({})",
                    sandbox.authority()
                ));
            }
        }
        unmet
    }

    /// Whether this task's own steps have met its closing condition.
    ///
    /// Narrow deliberately, and each narrowing is a case that would otherwise
    /// close a task that is not finished:
    ///
    /// - the whole command line, so `cargo test` does not match
    ///   `cargo test --no-run`, which compiles the tests and runs none;
    /// - `exit_code == Some(0)`, so a child killed by `SIGXCPU` — which has no
    ///   exit code at all — is the limit reporting a command that never
    ///   finished, rather than a success;
    /// - the caller's `steps`, which is this task's, so another task's green
    ///   test run is not this one's evidence.
    pub fn met_by(&self, steps: &[&ToolStep]) -> bool {
        let Some(closes_on) = &self.closes_on else {
            return false;
        };
        steps.iter().any(|step| {
            step.outcome
                .command
                .as_ref()
                .is_some_and(|result| result.exit_code == Some(0))
                && command_line(&step.call).as_deref() == Some(closes_on.as_str())
        })
    }

    /// The sandbox this plan *is*, once someone has approved it.
    ///
    /// Built out of what `session` already grants rather than out of the plan's
    /// own words: a plan cannot invent access, so every path it names keeps
    /// exactly the access the policy file gave it and a path the file never
    /// granted is simply absent. [`Plan::unmet`] refuses that plan before this
    /// is ever called, and this is the same rule applied a second time, where
    /// it bites.
    ///
    /// **It narrows the level too.** `files` are granted `Read` and `writes`
    /// `ReadWrite`, so a task that declared a file and did not declare a write
    /// may read it and cannot touch it, even where the policy file allows both.
    /// `network` and `enforcement` are still the session's: a plan declares
    /// neither.
    ///
    /// A path that does not exist yet cannot be a root — Landlock takes a file
    /// descriptor per root, so [`Sandbox::new`] requires it to be there. A read
    /// of one grants nothing, because there is nothing to read. A **write** to
    /// one grants its nearest existing ancestor, because creating a file is a
    /// write to its directory — and at the kernel rung a grant *is* a
    /// directory. That is wider than the plan's words and it is the honest
    /// resolution: the alternative is that no plan may ever create a file.
    pub fn narrow(&self, session: &Sandbox, job: JobId) -> Result<Sandbox, SandboxError> {
        let narrowed_egress = match (session.egress().is_empty(), self.egress.is_empty()) {
            (false, false) => self
                .egress
                .iter()
                .filter(|d| session.allows_domain(d))
                .cloned()
                .collect(),
            (false, true) => session.egress().to_vec(),
            (true, false) => self.egress.clone(),
            (true, true) => Vec::new(),
        };
        let mut policy = SandboxPolicy {
            paths: Vec::new(),
            commands: Vec::new(),
            network: session.network() && (self.network || !self.egress.is_empty()),
            egress: narrowed_egress,
            proxy: session.proxy().map(String::from),
            // The strictest of the two, which for this field is the session's
            // unless the plan asked for more. `unmet` has already refused a
            // plan asking for less, so this cannot widen even if it is reached
            // with one.
            enforcement: match (session.enforcement(), self.enforcement) {
                (Enforcement::Kernel, _) => Enforcement::Kernel,
                (_, Some(asked)) => asked,
                (session, None) => session,
            },
            // The session's, like `network` and `enforcement`: a plan declares
            // paths and commands, and what a child may *spend* is not something
            // it has words for. Whether it should is in
            // `RECORD/2026-09-01.what-the-audit-left.completed.md`.
            limits: session.limits(),
        };
        for file in &self.files {
            let path = std::path::Path::new(file);
            if !path_exists(session, path) {
                continue;
            }
            if session.access_for(path).is_some() {
                policy.allow(file, Access::Read);
            }
        }
        for file in &self.writes {
            let path = std::path::Path::new(file);
            let grant = match path_exists(session, path) {
                true => Some(std::path::PathBuf::from(file)),
                // Creating a file is a write to the directory that will hold it.
                false => nearest_existing(session, path),
            };
            if let Some(grant) = grant
                && session.access_for(&grant) == Some(Access::ReadWrite)
            {
                policy.allow(grant, Access::ReadWrite);
            }
        }
        for command in &self.commands {
            if session.commands().iter().any(|allowed| allowed == command) {
                policy.allow_command(command);
            }
        }
        Ok(Sandbox::new(&policy, session.base())?.under(Authority::Plan(job)))
    }

    /// Adds what a person put in at the gate, ignoring what is already there.
    ///
    /// The amendment is checked with [`Plan::unmet`] like any other plan, so
    /// the human at the gate can widen the plan up to the policy file and not
    /// past it — otherwise the gate is the policy and `luu.toml` is a
    /// suggestion.
    #[allow(clippy::too_many_arguments)]
    pub fn amend(
        &mut self,
        files: &[String],
        writes: &[String],
        commands: &[String],
        closes_on: Option<&str>,
        network: Option<bool>,
        egress: Option<&[String]>,
        enforcement: Option<Enforcement>,
    ) {
        for file in files {
            if !self.files.contains(file) {
                self.files.push(file.clone());
            }
        }
        for file in writes {
            if !self.writes.contains(file) {
                self.writes.push(file.clone());
            }
        }
        for command in commands {
            if !self.commands.contains(command) {
                self.commands.push(command.clone());
            }
        }
        // Replaced rather than merged: there is one closing condition, and a
        // person who types a second one is correcting the first.
        if let Some(closes_on) = closes_on.map(str::trim).filter(|it| !it.is_empty()) {
            self.closes_on = Some(closes_on.to_string());
        }
        // Replaced rather than merged, like `closes_on`: there is one answer,
        // and `unmet` runs over the amended plan, so a person who types the
        // lenient value at the gate is refused there exactly as a model would
        // have been.
        if enforcement.is_some() {
            self.enforcement = enforcement;
        }
        if let Some(net) = network {
            self.network = net;
        }
        if let Some(domains) = egress {
            for domain in domains {
                if !self.egress.contains(domain) {
                    self.egress.push(domain.clone());
                }
            }
            if !domains.is_empty() {
                self.network = true;
            }
        }
    }

    /// One line per part it has, for a human reading a run go by.
    pub fn describe(&self) -> String {
        let mut text = String::new();
        for (part, items) in [
            ("tasks", &self.tasks),
            ("files", &self.files),
            ("writes", &self.writes),
            ("commands", &self.commands),
        ] {
            if !items.is_empty() {
                text.push_str(&format!("  {part:<9}{}\n", items.join(" · ")));
            }
        }
        if self.network {
            text.push_str("  network  allowed\n");
        }
        if !self.egress.is_empty() {
            text.push_str(&format!("  egress   {}\n", self.egress.join(" · ")));
        }
        // Shown whenever the plan asked, including when it asked for what the
        // session already does: a plan the person is approving has to show
        // everything it decides, and "the same as the session" is a decision
        // once the field exists.
        if let Some(enforcement) = self.enforcement {
            text.push_str(&format!("  enforce  {}\n", enforcement.as_str()));
        }
        // Last, and named for what it does rather than for what it is: this is
        // the line that says the task may close without anyone present.
        if let Some(closes_on) = &self.closes_on {
            text.push_str(&format!(
                "  {:<9}{closes_on} (exit 0 closes it)\n",
                "closes on"
            ));
        }
        text
    }
}

/// Whether a path is there to be granted, resolved the way the sandbox does.
fn path_exists(session: &Sandbox, path: &std::path::Path) -> bool {
    absolute(session, path).exists()
}

/// The closest ancestor that exists, which is what a not-yet-created file needs
/// written. `None` only if nothing up to the root is there, which cannot happen
/// for a path under the base — and is not a grant either way.
fn nearest_existing(session: &Sandbox, path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut candidate = absolute(session, path);
    while !candidate.exists() {
        candidate = candidate.parent()?.to_path_buf();
    }
    Some(candidate)
}

fn absolute(session: &Sandbox, path: &std::path::Path) -> std::path::PathBuf {
    match path.is_absolute() {
        true => path.to_path_buf(),
        false => session.base().join(path),
    }
}

/// A plan as the model proposes it, before anyone has approved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub objective: String,
    #[serde(flatten)]
    pub plan: Plan,
}

/// Reads a proposal out of what the model said.
///
/// The same shape as [`crate::tools::parse_call`], and for the same reason:
/// how a model expresses a plan is a transport detail — a fenced block today, a
/// GBNF grammar next — and nothing above this line should care which.
///
/// `None` covers the ordinary case of a small model answering the planning call
/// in prose. The caller proposes the user's own ask instead; a gate that
/// disappeared because the model was vague would be the one failure mode this
/// whole mechanism exists to prevent.
pub fn parse_plan(text: &str) -> Option<Proposal> {
    let proposal: Proposal =
        crate::tools::fenced(text, "plan").and_then(|body| serde_json::from_str(body).ok())?;
    (!proposal.objective.trim().is_empty()).then_some(proposal)
}

/// Where a plan came from — which is not the same question as what is in it.
///
/// The gate's headline number is *how often a small model produces a plan worth
/// reading*, and without this nothing can answer it: a planning call that
/// answered in prose and one that emitted a well-formed block declaring nothing
/// arrive at the panel as the same empty plan. Guessing from emptiness is how a
/// model that declared `"files": []` gets counted as a model that could not
/// follow the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanSource {
    /// The planning call emitted a fenced `plan` block and it parsed.
    Model,
    /// It answered without one — the ordinary case for a 7B. The proposal is
    /// then the user's own ask, declaring nothing, and the gate still holds.
    Prose,
    /// No planning call happened: a script's `## task:` directives carry the
    /// plan, which is a person's approval written down in advance.
    Written,
}

/// Where a job is in its life: opened, and not yet closed.
///
/// Two variants and not four. `Proposed` and `Rejected` were states of a thing
/// that under the alternation is not a job — a plan is offered *inside* a draft
/// and an id is what approval hands out, so a refused plan never gets one and
/// has no state to be in. Both survive as [`crate::context::Round`], which is
/// where the number they existed to produce now lives. See
/// `RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Opened by a turn landing in it, and not closed. Its turns are the live
    /// ones, sent verbatim.
    ///
    /// Reads an older recording's `approved` too: a job that was approved and
    /// not closed is precisely one that is open, and the word is the only part
    /// of the claim that moved.
    #[serde(alias = "approved")]
    Open,
    /// Closed: its turns are folded to [`Job::summary`] from here on.
    Closed,
}

pub type TaskState = JobState;

/// Which authority closed a job.
///
/// On the close rather than inferred afterwards, because the ladder above the
/// person is only worth climbing if each rung can be *counted*: how often the
/// exit code closed a job a person would have left open, and how often a
/// person closed one it missed. A recording where both closes look the same
/// cannot answer either, and by then the sessions are on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosedBy {
    /// A person, at the gate. What every close before this existed was, which
    /// is why an absent value reads as this one.
    User,
    /// The job's own [`Plan::closes_on`], on an exit code of 0.
    ExitCode,
    /// A plan was approved out of it, which closes the draft it was proposed
    /// inside and opens the job in one transition.
    ///
    /// **The rung the alternation made common.** Before item 22 a draft was not
    /// a thing a session had, and every close was a person or an exit code;
    /// since it, most drafts end this way and nothing said so — a recording
    /// could not count how often exploration led to approved work, and the
    /// transcript called it the same thing as a draft somebody gave up on. See
    /// `RECORD/2026-09-21.the-alternation-on-the-page.completed.md`.
    Approval,
}

/// Which authority approved a job.
///
/// Beside [`ClosedBy`] and for the same reason: a rung above the person is only
/// worth climbing if each rung can be *counted*, and a recording where a
/// signed approval and an operator's look the same cannot say how often either
/// happened. See `RECORD/2026-09-04.signed-approvals.completed.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum ApprovedBy {
    /// A person at the gate, unsigned. What every approval before signatures
    /// existed was, which is why an absent value reads as this one.
    #[default]
    Operator,
    /// A signature that verified against a key `luu.toml` names.
    Key { name: String },
}

/// What a closed job leaves behind, counted once at the close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub text: String,
    pub tokens: u32,
    /// Which counter produced `tokens` — same reason [`crate::context::Turn`]
    /// carries one: two counters summed into one bar is not a measurement.
    pub counted_by: Counter,
    /// What this summary stands in for, counted at the close.
    ///
    /// `None` in a recording written before format 9, and that is not zero: a
    /// fold that saved nothing and a fold nobody measured are different claims.
    /// See `RECORD/2026-09-08.what-a-fold-writes-down.completed.md`.
    #[serde(default)]
    pub replaced: Option<Replaced>,
}

/// The turns a fold took out of the prompt, and what they were worth there.
///
/// Counted at the close with the counter that counted the summary, because the
/// saving is `tokens` subtracted from this one and two counters subtracted from
/// each other are not a saving. It cannot be recovered later either: by then the
/// window has moved, a resume may have changed the counter, and eviction may
/// have taken some of the same turns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Replaced {
    /// Named rather than counted: a reader months later cannot recover *which*
    /// turns stopped being sent, and the transcript can point at them.
    pub turns: Vec<crate::protocol::TurnId>,
    /// What they were worth in the prompt they are no longer in — the unit
    /// `Summary::tokens` is in, and the one the `history` bucket sums.
    pub tokens: u32,
    /// What the line that stands in for them costs, by the same counter at the
    /// same moment. The same number as [`Summary::tokens`], carried here
    /// because that is not what `job_closed` sends — the message carries the
    /// summary's *text* — and a reader that had to count it again would be a
    /// second implementation of the counter, disagreeing with the first
    /// wherever a real tokenizer is in use.
    pub summary_tokens: u32,
}

/// One piece of work: opened by a turn landing in it, run, closed.
///
/// **A draft is a job whose objective is known and whose plan is not**, which is
/// the whole of what `plan` being an `Option` says. The empty plan could not say
/// it: [`Plan::writes`] is documented as *a plan that declares none may not
/// write at all*, so a draft carrying `Plan::default()` through [`Plan::narrow`]
/// would be handed the strictest sandbox there is and refused its first read.
/// The gate reads [`JobState`]-independent state instead — a narrowing is built
/// at an approval and nowhere else — so a draft is a job for the window and is
/// not a job for the gate. See
/// `RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    /// What the user asked for, as they asked for it.
    pub objective: String,
    /// The plan it is held to, or `None` while it is a draft.
    ///
    /// `None` is not "no restriction" and is never read as one: nothing narrows
    /// a sandbox from this field, so a job without a plan runs under the
    /// session's own policy exactly as a jobless turn did before jobs were
    /// total.
    pub plan: Option<Plan>,
    pub state: JobState,
    /// Present from the close onwards. Dropped on a reopen, because a reopened
    /// job is being written again and the old account of it is not an account
    /// of what it will have been.
    pub summary: Option<Summary>,
    /// Who closed it. Beside the summary and dropped with it: a reopened job
    /// has not been closed by anyone.
    #[serde(default)]
    pub closed_by: Option<ClosedBy>,
    /// Who approved it, from the approval onwards. Absent while it is still a
    /// proposal, and absent in a record written before signatures existed —
    /// which reads as [`ApprovedBy::Operator`], because that is what it was.
    #[serde(default)]
    pub approved_by: Option<ApprovedBy>,
}

pub type Task = Job;

impl Job {
    /// A draft: the objective is what the user asked, and there is no plan
    /// because nobody has written one yet.
    ///
    /// It is open from the moment it exists, because the only thing that brings
    /// it into existence is a turn landing in it — see
    /// [`crate::context::Context::current_job`], where that derivation lives.
    pub fn draft(id: JobId, objective: impl Into<String>) -> Self {
        Self {
            id,
            objective: objective.into(),
            plan: None,
            state: JobState::Open,
            summary: None,
            closed_by: None,
            approved_by: None,
        }
    }

    /// A job an approval opened, holding the plan it was approved with and the
    /// authority that approved it.
    ///
    /// There is no `approve` on a job any more, and that is the shape rather
    /// than a rename: approval does not move a job from one state to another,
    /// it **closes** the draft and opens this one. A constructor is what that
    /// looks like in the type.
    pub fn planned(id: JobId, objective: impl Into<String>, plan: Plan, by: ApprovedBy) -> Self {
        Self {
            id,
            objective: objective.into(),
            plan: Some(plan),
            state: JobState::Open,
            summary: None,
            closed_by: None,
            approved_by: Some(by),
        }
    }

    /// Opened and not closed: the one job turns are attributed to.
    pub fn is_open(&self) -> bool {
        self.state == JobState::Open
    }

    /// Whether it is a draft — known objective, no plan.
    ///
    /// The question every caller that used to ask `state == Proposed` is
    /// actually asking, and it is answered by a field rather than by counting
    /// the job's position in the alternation: a fold that has to be counted to
    /// be understood is a fold a reader cannot check.
    pub fn is_draft(&self) -> bool {
        self.plan.is_none()
    }

    pub fn is_closed(&self) -> bool {
        self.state == JobState::Closed
    }

    /// Closes the job and writes its summary, once.
    ///
    /// `steps` is everything the job's turns did and `shown` is every fragment
    /// they were handed, both in order, and `turns` is how many turns it took.
    /// All three are facts about what happened; nothing the model said about
    /// what happened is used.
    /// `replaced` is what the job's turns were worth in the prompt, counted by
    /// the caller with this same counter — the fold's whole claim is the
    /// difference between it and the summary, so the two are counted together
    /// or not at all.
    pub fn close(
        &mut self,
        steps: &[&ToolStep],
        shown: &[&Fragment],
        turns: usize,
        counter: &dyn TokenCounter,
        by: ClosedBy,
        replaced: Option<Replaced>,
    ) {
        let text = summary_text(
            &self.objective,
            self.plan.as_ref(),
            steps,
            shown,
            turns,
            counter,
        );
        let tokens = counter.count(&text);
        self.summary = Some(Summary {
            tokens,
            counted_by: counter.id(),
            text,
            replaced: replaced.map(|replaced| Replaced {
                summary_tokens: tokens,
                ..replaced
            }),
        });
        self.state = JobState::Closed;
        self.closed_by = Some(by);
    }

    /// Reopens it: the fold stops applying and the turns are sent verbatim
    /// again. Not an undo — nothing was deleted — which is the whole reason
    /// closing was made an event.
    pub fn reopen(&mut self) {
        self.state = JobState::Open;
        self.summary = None;
        self.closed_by = None;
    }
}

/// The deterministic summary: the approved plan, what the job was shown, and
/// what the tools actually reported. Read `RECORD/2026-08-30.tasks-in-code.completed.md`
/// before adding prose to it — it lands in the write-once region, where being
/// wrong is unrecoverable for the rest of the session.
///
/// A fragment is quoted from the file's own bytes for that reason: it cannot be
/// a hallucination, and it is the thing the grounded probe showed the fold
/// losing — `RECORD/2026-08-30.the-fold-probe-run.completed.md` measured it and
/// `RECORD/2026-08-30.what-a-summary-should-carry.completed.md` argues the fix.
fn summary_text(
    objective: &str,
    plan: Option<&Plan>,
    steps: &[&ToolStep],
    shown: &[&Fragment],
    turns: usize,
    counter: &dyn TokenCounter,
) -> String {
    // A draft says so in its first line, and it is the line a reader of the
    // folded window reads. *What we looked at and what we decided* and *what we
    // then did* are different claims about the session, and a fold that headed
    // both with the same words would make the alternation unreadable at exactly
    // the point it is supposed to be legible. Everything below this is shared,
    // because a draft's turns run the ordinary loop under the session's own
    // policy — the record's guess that they "ran no tools" is true of a draft
    // nobody explored in and of no other.
    let mut text = match plan {
        Some(_) => format!("[job closed] {objective}\n"),
        None => format!("[draft closed] {objective}\n"),
    };
    if let Some(plan) = plan
        && !plan.tasks.is_empty()
    {
        text.push_str("approved plan:\n");
        for task in &plan.tasks {
            text.push_str(&format!("  - {task}\n"));
        }
    }

    let shown = quoted(shown, counter);
    if !shown.is_empty() {
        text.push_str(&format!("shown, from {turns} turn(s):\n"));
        for (fragment, tokens) in &shown {
            let lines = fragment.text.lines().count();
            text.push_str(&match tokens {
                Some(tokens) => format!("  {} ({lines} line(s), {tokens} tokens)\n", fragment.path),
                None => format!("  {} — over the cap, not kept\n", fragment.path),
            });
        }
    }

    let evidence = evidence(steps);
    match (evidence.is_empty(), shown.is_empty()) {
        // Said out loud rather than left off: "this task ran no tools" is a
        // fact about it, and a summary that simply stops reads like one that
        // was cut short.
        (true, true) => text.push_str(&format!(
            "no tools ran; {turns} turn(s) folded, their text not kept\n"
        )),
        // The same fact, without the half of that sentence which is no longer
        // true. A model reading "their text not kept" answers the question that
        // sentence asks — the probe's turn 18 refused rather than answered — so
        // it must not be said over a quote that is right there.
        (true, false) => text.push_str("no tools ran; the turns' own text is not kept\n"),
        (false, _) => {
            text.push_str(&format!("evidence, from {turns} turn(s):\n"));
            for line in &evidence {
                text.push_str(&format!("  {line}\n"));
            }
        }
    }

    for (fragment, tokens) in &shown {
        if tokens.is_some() {
            text.push_str(&format!("--- {}\n{}", fragment.path, fragment.text));
            if !fragment.text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("---\n");
        }
    }
    text
}

/// The distinct fragments the task was handed, in the order it saw them, each
/// with its token count — or `None` for one the cap left out.
///
/// Newest first while choosing, because the last thing a task was shown is the
/// likeliest to be what a later turn asks about; then rendered in the order
/// they were shown, because that is the order they were read in.
///
/// Whole or not at all: half a file quoted under a heading naming the whole one
/// is a summary nobody can account for, which is the same reason
/// [`crate::tools::MAX_OUTPUT_BYTES`] says so in the text when it cuts.
fn quoted<'a>(
    shown: &[&'a Fragment],
    counter: &dyn TokenCounter,
) -> Vec<(&'a Fragment, Option<u32>)> {
    let mut distinct: Vec<&Fragment> = Vec::new();
    for fragment in shown {
        if !distinct.iter().any(|seen| seen.path == fragment.path) {
            distinct.push(fragment);
        }
    }

    let mut kept: Vec<(&Fragment, Option<u32>)> =
        distinct.iter().map(|fragment| (*fragment, None)).collect();
    let mut budget = MAX_QUOTED_TOKENS;
    for entry in kept.iter_mut().rev() {
        let tokens = counter.count(&entry.0.text);
        if tokens <= budget {
            budget -= tokens;
            entry.1 = Some(tokens);
        }
    }
    kept
}

/// The distinct actions a task took, in order, each with what it reported.
///
/// Identical lines collapse rather than repeat: a loop that read the same file
/// eight times is one fact, and eight copies of it in the prefix is eight
/// copies of it paid for on every later call.
fn evidence(steps: &[&ToolStep]) -> Vec<String> {
    let mut lines: Vec<(String, usize)> = Vec::new();
    for step in steps {
        let status = step.outcome.error.as_deref().unwrap_or("ok");
        let line = match command_line(&step.call) {
            Some(target) => format!("{} {target} — {status}", step.call.name),
            None => format!("{} — {status}", step.call.name),
        };
        match lines.iter_mut().find(|(seen, _)| *seen == line) {
            Some((_, count)) => *count += 1,
            None => lines.push((line, 1)),
        }
    }

    let over = lines.len().saturating_sub(MAX_EVIDENCE_LINES);
    let mut rendered: Vec<String> = lines
        .into_iter()
        .take(MAX_EVIDENCE_LINES)
        .map(|(line, count)| match count {
            1 => line,
            n => format!("{line} (x{n})"),
        })
        .collect();
    if over > 0 {
        rendered.push(format!("... and {over} more"));
    }
    rendered
}

/// What a call acted on, in as few tokens as say it: the path, or the command
/// line. Arguments a reader cannot act on are left out — the summary is
/// evidence, not a replay.
///
/// Public because [`Plan::met_by`] matches against the same rendering the
/// summary shows. Two renderings of one command line would drift, and the day
/// they drift a task stops closing for a reason nobody can see.
pub fn command_line(call: &ToolCall) -> Option<String> {
    if let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) {
        return Some(path.to_string());
    }
    let argv = call.arguments.get("argv")?.as_array()?;
    let line = argv
        .iter()
        .filter_map(|arg| arg.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ApproximateCounter;
    use crate::sandbox::{Enforcement, PathRule, SandboxPolicy, Verdict};
    use crate::tools::ToolOutcome;

    fn step(name: &str, arguments: serde_json::Value, error: Option<&str>) -> ToolStep {
        ToolStep {
            text: String::new(),
            call: ToolCall {
                name: name.into(),
                arguments,
            },
            outcome: ToolOutcome {
                verdict: Verdict::deny("test"),
                output: String::new(),
                error: error.map(str::to_string),
                truncated: false,
                command: None,
            },
            duration_ms: 0,
            asked: None,
        }
    }

    /// The same, for a command that actually ran: `met_by` reads the exit code
    /// and nothing else, so a step without one can never meet a condition.
    fn ran(command: &str, args: &[&str], exit_code: Option<i32>, signal: Option<i32>) -> ToolStep {
        let argv: Vec<&str> = std::iter::once(command)
            .chain(args.iter().copied())
            .collect();
        let mut step = step("run_command", serde_json::json!({"argv": argv}), None);
        step.outcome.command = Some(crate::tools::CommandResult {
            exit_code,
            signal,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 1,
        });
        step
    }

    fn closing_on(closes_on: &str) -> Plan {
        Plan {
            commands: vec!["cargo".into()],
            closes_on: Some(closes_on.into()),
            ..Plan::default()
        }
    }

    #[test]
    fn a_closing_condition_is_met_by_the_whole_command_line_and_an_exit_of_zero() {
        let plan = closing_on("cargo test");
        let met = ran("cargo", &["test"], Some(0), None);
        assert!(plan.met_by(&[&met]), "the command it names, green");
    }

    #[test]
    fn a_longer_command_line_is_a_different_command() {
        let plan = closing_on("cargo test");
        // Compiles the tests and runs none of them. A prefix match would close
        // the task here, and the difference between these two command lines is
        // the whole difference between the work being done and not.
        let compiled = ran("cargo", &["test", "--no-run"], Some(0), None);
        assert!(!plan.met_by(&[&compiled]));
    }

    #[test]
    fn a_failing_run_leaves_the_task_open() {
        let plan = closing_on("cargo test");
        assert!(!plan.met_by(&[&ran("cargo", &["test"], Some(1), None)]));
    }

    #[test]
    fn a_child_the_limits_killed_is_not_a_success() {
        let plan = closing_on("cargo test");
        // No exit code at all, which is exactly when `signal` is `Some`: the
        // `[sandbox.limits]` rung reporting a command that never finished. The
        // one case a "no error" test would have got wrong.
        let killed = ran("cargo", &["test"], None, Some(24));
        assert!(!plan.met_by(&[&killed]));
    }

    #[test]
    fn a_plan_that_declares_no_condition_is_never_met() {
        let plan = Plan {
            commands: vec!["cargo".into()],
            ..Plan::default()
        };
        assert!(
            !plan.met_by(&[&ran("cargo", &["test"], Some(0), None)]),
            "which is every plan a model writes: the person stays the authority",
        );
    }

    #[test]
    fn a_condition_the_plan_may_not_run_is_refused_at_the_gate() {
        let session = session();
        let plan = Plan {
            commands: vec!["cargo".into()],
            closes_on: Some("git status".into()),
            ..Plan::default()
        };
        let unmet = plan.unmet(&session);
        assert!(
            unmet.iter().any(|line| line.starts_with("closes_on ")),
            "a condition that can never be met looks like one that will: {unmet:?}",
        );
    }

    #[test]
    fn the_gate_is_where_a_closing_condition_comes_from() {
        let mut plan = Plan {
            commands: vec!["cargo".into()],
            ..Plan::default()
        };
        assert!(plan.closes_on.is_none(), "the model was never asked");
        plan.amend(&[], &[], &[], Some("  cargo test  "), None, None, None);
        assert_eq!(plan.closes_on.as_deref(), Some("cargo test"));
        // One condition, and a person who types a second is correcting the
        // first — unlike the lists beside it, which accumulate.
        plan.amend(&[], &[], &[], Some("cargo clippy"), None, None, None);
        assert_eq!(plan.closes_on.as_deref(), Some("cargo clippy"));
        plan.amend(&[], &[], &[], Some("   "), None, None, None);
        assert_eq!(
            plan.closes_on.as_deref(),
            Some("cargo clippy"),
            "an empty box is not an instruction to clear it",
        );
    }

    #[test]
    fn the_summary_is_the_plan_and_the_evidence_and_nothing_the_model_said() {
        let mut task = Task::planned(
            2,
            "add a --dry-run flag",
            Plan {
                tasks: vec!["read the CLI".into(), "add the flag".into()],
                files: vec!["crates/luu/src/lib.rs".into()],
                writes: vec![],
                commands: vec!["cargo".into()],
                closes_on: None,
                network: false,
                ..Plan::default()
            },
            ApprovedBy::Operator,
        );
        let steps = [
            step("read_file", serde_json::json!({"path": "src/lib.rs"}), None),
            step(
                "run_command",
                serde_json::json!({"argv": ["cargo", "test"]}),
                Some("cargo exited with 1"),
            ),
        ];
        task.close(
            &steps.iter().collect::<Vec<_>>(),
            &[],
            3,
            &ApproximateCounter,
            ClosedBy::User,
            None,
        );

        let text = &task.summary.as_ref().unwrap().text;
        assert!(text.contains("add a --dry-run flag"));
        assert!(text.contains("- read the CLI"));
        assert!(text.contains("read_file src/lib.rs — ok"));
        assert!(
            text.contains("run_command cargo test — cargo exited with 1"),
            "a non-zero exit is the evidence, not a detail: {text}"
        );
        assert!(task.is_closed());
    }

    #[test]
    fn repeated_identical_actions_collapse_rather_than_repeat() {
        let mut task = Task::planned(1, "look around", Plan::default(), ApprovedBy::Operator);
        let steps: Vec<ToolStep> = (0..8)
            .map(|_| step("list_dir", serde_json::json!({"path": "."}), None))
            .collect();
        task.close(
            &steps.iter().collect::<Vec<_>>(),
            &[],
            8,
            &ApproximateCounter,
            ClosedBy::User,
            None,
        );

        let text = &task.summary.as_ref().unwrap().text;
        assert!(text.contains("list_dir . — ok (x8)"), "{text}");
        assert_eq!(
            text.lines().filter(|l| l.contains("list_dir")).count(),
            1,
            "eight copies in the prefix is eight copies paid for every later call",
        );
    }

    #[test]
    fn a_task_that_ran_no_tools_says_so_rather_than_stopping() {
        let mut task = Task::planned(
            1,
            "explain the design",
            Plan::default(),
            ApprovedBy::Operator,
        );
        task.close(&[], &[], 4, &ApproximateCounter, ClosedBy::User, None);
        let text = &task.summary.as_ref().unwrap().text;
        assert!(text.contains("no tools ran"), "{text}");
        assert!(text.contains("4 turn(s) folded"), "{text}");
    }

    fn fragment(path: &str, text: &str) -> Fragment {
        Fragment {
            path: path.into(),
            text: text.into(),
        }
    }

    #[test]
    fn what_the_task_was_shown_is_quoted_from_the_file_and_not_described() {
        let mut task = Task::planned(
            3,
            "work out what the sandbox policy grants",
            Plan::default(),
            ApprovedBy::Operator,
        );
        let shown = [fragment(
            "luu.toml:1-3",
            "[sandbox]\ncommands = [\"cargo\", \"rg\"]\n",
        )];
        task.close(
            &[],
            &shown.iter().collect::<Vec<_>>(),
            5,
            &ApproximateCounter,
            ClosedBy::User,
            None,
        );

        let text = &task.summary.as_ref().unwrap().text;
        assert!(text.contains("shown, from 5 turn(s):"), "{text}");
        assert!(text.contains("luu.toml:1-3 (2 line(s),"), "{text}");
        assert!(
            text.contains("commands = [\"cargo\", \"rg\"]"),
            "the file's own bytes, which cannot be a hallucination: {text}",
        );
        assert!(
            !text.contains("their text not kept"),
            "the half of that sentence which the quote makes false is what made \
             the probe's turn 18 refuse a question it could answer: {text}",
        );
    }

    #[test]
    fn the_same_fragment_shown_twice_is_quoted_once() {
        let mut task = Task::planned(1, "read it twice", Plan::default(), ApprovedBy::Operator);
        let twice = [
            fragment("luu.toml:1-2", "[sandbox]\npaths = []\n"),
            fragment("luu.toml:1-2", "[sandbox]\npaths = []\n"),
        ];
        task.close(
            &[],
            &twice.iter().collect::<Vec<_>>(),
            2,
            &ApproximateCounter,
            ClosedBy::User,
            None,
        );

        let text = &task.summary.as_ref().unwrap().text;
        assert_eq!(
            text.matches("[sandbox]").count(),
            1,
            "twice in the prefix is twice paid for on every later call: {text}",
        );
    }

    #[test]
    fn over_the_cap_the_newest_is_kept_and_the_rest_are_named() {
        let mut task = Task::planned(1, "read a lot", Plan::default(), ApprovedBy::Operator);
        let big = "a word ".repeat(MAX_QUOTED_TOKENS as usize);
        let shown = [
            fragment("old.rs", &big),
            fragment("new.rs", "the last thing it was shown\n"),
        ];
        task.close(
            &[],
            &shown.iter().collect::<Vec<_>>(),
            4,
            &ApproximateCounter,
            ClosedBy::User,
            None,
        );

        let text = &task.summary.as_ref().unwrap().text;
        assert!(
            text.contains("old.rs — over the cap, not kept"),
            "what was dropped is named rather than silently absent: {text}",
        );
        assert!(!text.contains("--- old.rs"), "{text}");
        assert!(
            text.contains("the last thing it was shown"),
            "the newest is the likeliest to be asked about: {text}",
        );
    }

    #[test]
    fn reopening_drops_the_summary_without_recovering_anything() {
        let mut task = Task::planned(1, "x", Plan::default(), ApprovedBy::Operator);
        task.close(&[], &[], 1, &ApproximateCounter, ClosedBy::User, None);
        task.reopen();
        assert!(task.is_open());
        assert!(
            task.summary.is_none(),
            "the fold stops applying; the turns were never deleted to recover",
        );
    }

    #[test]
    fn a_proposal_is_read_out_of_a_fenced_block() {
        let proposal = parse_plan(
            "I will need to touch the CLI.\n\n             ```plan\n             {\"objective\": \"add a --dry-run flag\", \"steps\": [\"read the CLI\"],              \"files\": [\"crates/luu/src/lib.rs\"], \"commands\": [\"cargo\"]}\n             ```",
        )
        .unwrap();

        assert_eq!(proposal.objective, "add a --dry-run flag");
        assert_eq!(proposal.plan.files, ["crates/luu/src/lib.rs"]);
        assert_eq!(proposal.plan.commands, ["cargo"]);
    }

    #[test]
    fn a_proposal_may_declare_nothing_at_all() {
        let proposal = parse_plan("```plan\n{\"objective\": \"explain the design\"}\n```").unwrap();
        assert_eq!(proposal.objective, "explain the design");
        assert!(proposal.plan.tasks.is_empty());
        assert!(proposal.plan.files.is_empty());
    }

    #[test]
    fn prose_is_not_a_plan_and_says_so_by_returning_nothing() {
        // The ordinary answer from a small model, and the caller's cue to
        // propose the user's own ask instead. A gate that vanished here would
        // be the failure this mechanism exists to prevent.
        assert!(parse_plan("I'll read the CLI and then add the flag.").is_none());
        assert!(parse_plan("```plan\n{\"objective\": \"  \"}\n```").is_none());
        assert!(parse_plan("```plan\nnot json\n```").is_none());
    }

    #[test]
    fn a_plan_asking_for_more_than_the_sandbox_grants_is_refused_by_name() {
        let base = std::env::current_dir().unwrap();
        let sandbox = Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new(".", Access::Read)],
                commands: vec!["cargo".into()],
                ..SandboxPolicy::default()
            },
            &base,
        )
        .unwrap();

        let plan = Plan {
            tasks: vec![],
            files: vec!["Cargo.toml".into(), "/etc/passwd".into()],
            writes: vec![],
            commands: vec!["cargo".into(), "curl".into()],
            closes_on: None,
            network: false,
            ..Plan::default()
        };
        let unmet = plan.unmet(&sandbox);

        assert_eq!(unmet.len(), 2, "{unmet:?}");
        assert!(unmet[0].contains("/etc/passwd"));
        assert!(unmet[1].contains("curl"));
    }

    /// The session's sandbox for the narrowing tests: read-write on the tree,
    /// two commands, which is roughly what `luu.toml` grants.
    ///
    /// `best-effort` because these tests are about *who granted what*, and
    /// under `kernel` every `prepare_command` on a machine without Landlock is
    /// denied before the allowlist is ever consulted — which would make the
    /// assertions below pass or fail for a reason that is not theirs.
    fn session() -> Sandbox {
        Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new(".", Access::ReadWrite)],
                commands: vec!["cargo".into(), "git".into()],
                enforcement: Enforcement::BestEffort,
                ..SandboxPolicy::default()
            },
            &std::env::current_dir().unwrap(),
        )
        .unwrap()
    }

    /// The same session, held to the kernel rather than doing its best.
    fn strict_session() -> Sandbox {
        Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new(".", Access::ReadWrite)],
                commands: vec!["cargo".into(), "git".into()],
                enforcement: Enforcement::Kernel,
                ..SandboxPolicy::default()
            },
            &std::env::current_dir().unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn a_plan_that_asks_for_nothing_is_held_the_way_the_session_is() {
        for session in [session(), strict_session()] {
            let plan = Plan {
                commands: vec!["cargo".into()],
                ..Plan::default()
            };
            assert!(plan.unmet(&session).is_empty());
            assert_eq!(
                plan.narrow(&session, 1).unwrap().enforcement(),
                session.enforcement(),
                "`None` is what every plan written before the field meant",
            );
        }
    }

    #[test]
    fn a_plan_may_tighten_enforcement_without_asking_anyone() {
        let session = session();
        assert_eq!(session.enforcement(), Enforcement::BestEffort);
        let plan = Plan {
            commands: vec!["cargo".into()],
            enforcement: Some(Enforcement::Kernel),
            ..Plan::default()
        };

        assert!(
            plan.unmet(&session).is_empty(),
            "asking for more is narrowing, and narrowing needs no permission",
        );
        assert_eq!(
            plan.narrow(&session, 1).unwrap().enforcement(),
            Enforcement::Kernel,
            "one job held by the kernel inside a session that does its best",
        );
    }

    #[test]
    fn a_plan_may_not_loosen_enforcement_and_the_gate_says_so() {
        let session = strict_session();
        let plan = Plan {
            commands: vec!["cargo".into()],
            enforcement: Some(Enforcement::BestEffort),
            ..Plan::default()
        };

        let unmet = plan.unmet(&session);
        assert_eq!(unmet.len(), 1, "{unmet:?}");
        assert!(
            unmet[0].starts_with("enforcement best-effort:"),
            "{unmet:?}"
        );
        assert!(
            unmet[0].contains("the sandbox policy"),
            "a denial says which of the two refused: {unmet:?}",
        );
        // And it cannot be reached past the gate either: `narrow` takes the
        // strictest of the two, so a plan that was never checked still does not
        // widen the policy file.
        assert_eq!(
            plan.narrow(&session, 1).unwrap().enforcement(),
            Enforcement::Kernel,
        );
    }

    #[test]
    fn the_gate_can_tighten_a_plan_the_model_wrote() {
        let mut plan = Plan {
            commands: vec!["cargo".into()],
            ..Plan::default()
        };
        plan.amend(&[], &[], &[], None, None, None, Some(Enforcement::Kernel));
        assert_eq!(plan.enforcement, Some(Enforcement::Kernel));
        assert!(plan.describe().contains("enforce  kernel"));

        // And the other direction is refused at the gate exactly as it is in a
        // script: the amendment is checked like any other plan.
        let mut loosened = Plan::default();
        loosened.amend(
            &[],
            &[],
            &[],
            None,
            None,
            None,
            Some(Enforcement::BestEffort),
        );
        assert_eq!(loosened.unmet(&strict_session()).len(), 1);
    }

    #[test]
    fn an_approved_plan_grants_what_it_named_and_nothing_else() {
        let session = session();
        let plan = Plan {
            tasks: vec![],
            files: vec!["Cargo.toml".into()],
            writes: vec![],
            commands: vec!["cargo".into()],
            closes_on: None,
            network: false,
            ..Plan::default()
        };
        let narrowed = plan.narrow(&session, 1).unwrap();

        assert!(
            narrowed
                .check_path(std::path::Path::new("Cargo.toml"), Access::Read)
                .verdict
                .allowed,
        );
        let refused = narrowed.check_path(std::path::Path::new("src/job.rs"), Access::Read);
        assert!(!refused.verdict.allowed);
        assert!(
            refused.verdict.rule.contains("the approved plan for job 1"),
            "a denial has to say which authority refused: {}",
            refused.verdict.rule,
        );
        assert!(
            session
                .check_path(std::path::Path::new("src/job.rs"), Access::Read)
                .verdict
                .allowed,
            "the same file is still readable under the policy the task narrows",
        );

        assert!(narrowed.prepare_command("cargo").is_ok());
        let refused = narrowed.prepare_command("git").expect_err("not planned");
        assert!(
            refused.rule.contains("the approved plan for job 1"),
            "{refused:?}"
        );
    }

    /// Narrowing cuts the level too: a file the plan only says it will *read*
    /// is read-only inside the task, even where the policy grants both. Before
    /// `writes` existed a plan could not say which it meant, so every task ran
    /// with write access to everything it named.
    #[test]
    fn a_file_the_plan_only_reads_cannot_be_written() {
        let plan = Plan {
            files: vec!["Cargo.toml".into()],
            ..Plan::default()
        };
        let narrowed = plan.narrow(&session(), 1).unwrap();
        let path = std::path::Path::new("Cargo.toml");

        assert!(narrowed.check_path(path, Access::Read).verdict.allowed);
        let refused = narrowed.check_path(path, Access::ReadWrite);
        assert!(!refused.verdict.allowed);
        assert!(
            refused.verdict.rule.contains("the approved plan for job 1"),
            "{}",
            refused.verdict.rule,
        );
        assert!(
            session()
                .check_path(path, Access::ReadWrite)
                .verdict
                .allowed,
            "the policy still grants it: the task is what refuses",
        );
    }

    #[test]
    fn a_declared_write_is_granted_read_write() {
        let plan = Plan {
            writes: vec!["Cargo.toml".into()],
            ..Plan::default()
        };
        let narrowed = plan.narrow(&session(), 1).unwrap();

        assert!(
            narrowed
                .check_path(std::path::Path::new("Cargo.toml"), Access::ReadWrite)
                .verdict
                .allowed,
            "writing implies reading: `edit_file` reads before it replaces",
        );
    }

    /// The check `unmet` exists for and used to miss: a plan that says it will
    /// change a file under a read-only root is refused at the gate rather than
    /// four turns in, at the call.
    #[test]
    fn a_write_into_a_read_only_root_is_refused_at_the_gate() {
        let read_only = Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new(".", Access::Read)],
                commands: Vec::new(),
                enforcement: Enforcement::BestEffort,
                ..SandboxPolicy::default()
            },
            &std::env::current_dir().unwrap(),
        )
        .unwrap();

        let plan = Plan {
            files: vec!["Cargo.toml".into()],
            writes: vec!["Cargo.toml".into()],
            ..Plan::default()
        };
        let unmet = plan.unmet(&read_only);

        assert_eq!(unmet.len(), 1, "{unmet:?}");
        assert!(unmet[0].starts_with("write Cargo.toml:"), "{unmet:?}");
    }

    /// A plan that says it will create a file: the path is not there to be
    /// granted, so what it gets is the directory that will hold it — which is
    /// what creating a file actually needs, and what the kernel rung can
    /// express at all.
    #[test]
    fn a_write_to_a_file_that_does_not_exist_grants_its_directory() {
        let plan = Plan {
            writes: vec!["src/not-written-yet.rs".into()],
            ..Plan::default()
        };
        let narrowed = plan.narrow(&session(), 1).unwrap();

        assert!(
            narrowed
                .check_path(
                    std::path::Path::new("src/not-written-yet.rs"),
                    Access::ReadWrite
                )
                .verdict
                .allowed,
        );
        assert!(
            !narrowed
                .check_path(std::path::Path::new("Cargo.toml"), Access::Read)
                .verdict
                .allowed,
            "the directory it needs, not the tree above it",
        );
    }

    /// And a read of a file that is not there grants nothing: there is nothing
    /// to read, and a plan naming one used to make the whole task unresolvable.
    #[test]
    fn a_read_of_a_file_that_does_not_exist_is_not_a_grant_and_not_an_error() {
        let plan = Plan {
            files: vec!["never-existed.md".into(), "Cargo.toml".into()],
            ..Plan::default()
        };
        let narrowed = plan.narrow(&session(), 1).expect("resolves anyway");

        assert!(
            narrowed
                .check_path(std::path::Path::new("Cargo.toml"), Access::Read)
                .verdict
                .allowed,
        );
    }

    /// A plan that declares nothing grants nothing. That is the point of
    /// narrowing and it is also its sharpest edge, so it is pinned here.
    #[test]
    fn a_plan_that_declares_nothing_narrows_to_nothing() {
        let narrowed = Plan::default().narrow(&session(), 4).unwrap();

        assert!(
            !narrowed
                .check_path(std::path::Path::new("Cargo.toml"), Access::Read)
                .verdict
                .allowed,
        );
        assert!(narrowed.prepare_command("cargo").is_err());
    }

    /// What a person adds at the gate, and what they cannot add: the amendment
    /// is a plan like any other and `unmet` is what refuses it.
    #[test]
    fn an_amendment_widens_the_plan_and_is_still_checked_against_the_file() {
        let session = session();
        let mut plan = Plan {
            files: vec!["Cargo.toml".into()],
            ..Plan::default()
        };
        plan.amend(
            &["src/job.rs".into(), "Cargo.toml".into()],
            &[],
            &["git".into()],
            None,
            None,
            None,
            None,
        );

        assert_eq!(plan.files, ["Cargo.toml", "src/job.rs"], "no duplicate");
        assert!(plan.unmet(&session).is_empty());

        let narrowed = plan.narrow(&session, 1).unwrap();
        assert!(
            narrowed
                .check_path(std::path::Path::new("src/job.rs"), Access::Read)
                .verdict
                .allowed,
            "the file the person added at the gate is in the task's sandbox",
        );

        let mut past_the_file = Plan::default();
        past_the_file.amend(
            &["/etc/passwd".into()],
            &[],
            &["curl".into()],
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            past_the_file.unmet(&session).len(),
            2,
            "the human at the gate widens up to the policy file and not past it",
        );
    }

    #[test]
    fn a_plan_without_network_narrows_to_no_network() {
        let session = Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new(".", Access::ReadWrite)],
                commands: vec!["cargo".into()],
                network: true,
                enforcement: Enforcement::BestEffort,
                ..SandboxPolicy::default()
            },
            &std::env::current_dir().unwrap(),
        )
        .unwrap();

        assert!(session.network(), "session allows network");

        let plan = Plan {
            commands: vec!["cargo".into()],
            network: false,
            ..Plan::default()
        };
        let narrowed = plan.narrow(&session, 1).unwrap();
        assert!(!narrowed.network(), "plan without network gets no network");
    }

    #[test]
    fn a_plan_with_network_grants_network_if_session_allows() {
        let session = Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new(".", Access::ReadWrite)],
                commands: vec!["cargo".into()],
                network: true,
                enforcement: Enforcement::BestEffort,
                ..SandboxPolicy::default()
            },
            &std::env::current_dir().unwrap(),
        )
        .unwrap();

        let plan = Plan {
            commands: vec!["cargo".into()],
            network: true,
            ..Plan::default()
        };
        assert!(plan.unmet(&session).is_empty());
        let narrowed = plan.narrow(&session, 1).unwrap();
        assert!(narrowed.network(), "plan with network gets network");
    }

    #[test]
    fn a_plan_with_network_is_unmet_if_session_forbids_it() {
        let session = Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new(".", Access::ReadWrite)],
                commands: vec!["cargo".into()],
                network: false,
                enforcement: Enforcement::BestEffort,
                ..SandboxPolicy::default()
            },
            &std::env::current_dir().unwrap(),
        )
        .unwrap();

        let mut plan = Plan {
            commands: vec!["cargo".into()],
            ..Plan::default()
        };
        plan.amend(&[], &[], &[], None, Some(true), None, None);
        assert!(plan.network);

        let unmet = plan.unmet(&session);
        assert!(
            unmet.iter().any(|line| line.starts_with("network:")),
            "unmet should report network refusal: {unmet:?}",
        );
    }

    #[test]
    fn a_plan_with_egress_narrows_domains_and_unmet_checks_them() {
        let session = Sandbox::new(
            &SandboxPolicy {
                paths: vec![PathRule::new(".", Access::ReadWrite)],
                commands: vec!["cargo".into()],
                network: true,
                egress: vec!["crates.io".into(), "*.github.com".into()],
                proxy: Some("http://127.0.0.1:8888".into()),
                enforcement: Enforcement::BestEffort,
                ..SandboxPolicy::default()
            },
            &std::env::current_dir().unwrap(),
        )
        .unwrap();

        let plan = Plan {
            commands: vec!["cargo".into()],
            network: true,
            egress: vec!["crates.io".into(), "api.github.com".into()],
            ..Plan::default()
        };
        assert!(plan.unmet(&session).is_empty());

        let narrowed = plan.narrow(&session, 1).unwrap();
        assert_eq!(narrowed.egress(), &["crates.io", "api.github.com"]);
        assert_eq!(narrowed.proxy(), Some("http://127.0.0.1:8888"));
        assert!(narrowed.allows_domain("crates.io"));
        assert!(!narrowed.allows_domain("evil.com"));

        let plan_forbidden = Plan {
            commands: vec!["cargo".into()],
            network: true,
            egress: vec!["evil.com".into()],
            ..Plan::default()
        };
        let unmet = plan_forbidden.unmet(&session);
        assert!(
            unmet.iter().any(|line| line.contains("evil.com")),
            "evil.com should be unmet: {unmet:?}",
        );
    }
}
