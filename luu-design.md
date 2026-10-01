# luu

Local-first AI agent written in Rust that orchestrates calls to models: optimized for local inference (limited context, tight token budget), and free to reach a remote model when the user names one — never as a fallback. See [`RECORD/2026-09-01.local-first.completed.md`](RECORD/2026-09-01.local-first.completed.md) for what "local-first" obligates and what it costs.

This file is the design as it stands now, and is rewritten as decisions change. The dated
reasoning behind them — including the answers that were wrong first — lives in [`RECORD/`](RECORD/),
which is append-only. What is *planned and not yet true* lives in [`ROADMAP/`](ROADMAP/), one
directory per revision; nothing there is an answer to what this file answers. See [AGENTS.md](AGENTS.md).

## Goals

- **Optimize the context window** for local models (7B–32B, 8K–32K context) — the key differentiator vs. existing solutions that don't do this well.
- **Constrain resource access**: configurable folders and commands the agent can reach, checked in our own code *and* held by the kernel (Landlock + seccomp) for anything that runs as a subprocess.
- **Container mode**: option to run inside a container to isolate system access.
- **CLI mode**: direct terminal usage.
- **VSCode integration**: a chat tab similar to Copilot Chat.

## Jobs (and Model Tasks)

A session is an **alternation of jobs** (historically called tasks), not a mode the agent is in. To avoid confusion with model checklist items, what was previously called a task is now a **job** (the bounded operational container: objective, gated approval, sandbox boundary, and compaction fold), while **tasks** designates the checklist items / steps emitted by models inside a plan (`plan.tasks`). A session:

```
job 1  draft   objective: what the user asked, as they asked it
               turns: exploring, on the floor — the policy file, reads only
               approve → close and fold, and open job 2

job 2  plan    objective: the approved plan
               turns: the work, under the plan's own sandbox
               close → fold; the next turn opens the next draft
```

**Every turn belongs to a job, from the session's first turn.** A turn landing
where nothing is open **opens a draft** to land in, so `Turn::job` is total and
the turns that used to belong to nothing have somewhere to fold to. A **draft is
a job whose objective is known and whose plan is not** — `Job::plan` is the
`Option`, and it is the one that matters: the empty plan is not *no restriction*
but the strictest plan there is, so a draft carrying `Plan::default()` through
`Plan::narrow` would be refused its first `read_file`. Nothing narrows a sandbox
from a draft; a draft is a job for the window and is not a job for the gate.
What holds a draft's turns is therefore not a plan but **the floor**, which is
the session's own sandbox minus the write bit — see below.

**Approving *is* closing.** It is not a transition inside a job — it ends one and
opens the next, whose objective is the approved plan. So the draft folds at the
approval and its summary becomes the plan's context, in the order a reader wants
it: *this is what we looked at, this is what we decided, this is what we are now
doing.* The negative decisions — `decline`, `more changes`, and a prompt sent
instead of an answer — are the same answer, **not yet**: they end nothing and
keep you in the job you are in. At most one job is open, everything behind it is
a fold, and the window in steady state is *N small summaries plus the open job's
turns*.

**A job opens when a turn lands in it, and the current job is derived** — the
last job not closed — rather than stored, because a stored field would disagree
with the jobs the first time a resume rebuilt one and not the other. Two things
fall out of that one rule: a draft nobody asked anything in is never opened, so
it cannot fold to an empty summary; and a reopen is unambiguous exactly when
nothing has been asked since the close, which is the only case it is allowed in.

**The gate has two doors and neither is automatic.** The model suggests a plan
when a drafting turn's answer carries a plan block, or a person asks for one with
`request_plan` — and what that planning call reads is **the draft**, because *the
plan is what the draft compacts to*. It used to read a held prompt, before
anything had been looked at, which is a small model's worst case. No prompt is
held any more: a prompt runs, in the draft, always.

**And the page reads the alternation, on both surfaces.** A fold says which
half it was — *draft 3* or *job 4*, from the same `!plan` rule the live-job
block uses — and why it ended, which is now three rungs and not two:
`ClosedBy::Approval` beside `User` and `ExitCode`, because under the alternation
most drafts end by having a plan approved out of them and a recording that
called that *a person folded it* could not count either. `protocol::VERSION` 8
and `record::FORMAT` 17 carry it, the first bump of each taken for a **value**
rather than a line. The transcript keeps both halves rather than merging a
draft's fold into the job it became: a draft's turns happened, they are what the
plan was written from, and hiding them would be shortest exactly where somebody
is auditing what was approved.

Two things the page was getting wrong fell out of writing that. **A draft's
turns were loose**: `draft_opened` carries the turn it was opened to hold —
deliberately, because a turn that opens a draft is in a job that did not exist
when it started — and the page dropped it, so live, a draft's turns belonged to
nothing and its fold never appeared, while the same session reloaded got them
back from the store with their job on them. **And `reopen` was offered
everywhere**: `Context::reopen_job` takes the last job and no other, and item 22
shrank that window to *before your next prompt*, since every prompt opens a
draft. The page now offers it only where the server would take it, and the
refusal — which said *job N is not closed* about a job the person had just
watched fold — says which of the two reasons it is. See
[`RECORD/2026-09-21.the-alternation-on-the-page.completed.md`](RECORD/2026-09-21.the-alternation-on-the-page.completed.md).

**A proposal is not a job and has no id**, because an id is what approval hands
out — so a plan that is declined never acquires one, and `JobState` is `Open` and
`Closed` with nothing else in it. Where `Proposed` and `Rejected` went is
`Context::rounds`: every plan put up and what answered it, which is the number
the refused state existed to produce — *how many rounds a plan takes before one
is approved*. See
[`RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md`](RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md)
and [`RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`](RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md) §7th–8th.

**A drafting turn runs on the floor: the policy file with every grant it made
downgraded to a read.** For one day it ran on the policy file itself — on this
repository `.` at `read-write` plus `cargo`, `rustc` and `git` — so an
exploratory turn could edit the tree and run the build with nobody having
approved anything, where before item 22 the first prompt of a session waited
behind the gate. The floor closes that: **exploration is exploration, and the
gate is what unlocks the write bit.** A draft still reads everything the policy
grants and still runs `rg` and `ls`, because what a command may *do* is decided
by the paths it may touch; `cargo test` in a draft now fails when it writes
`target/`, which is the intended answer and not a gap.

Mechanically it is one expression: `serve` picks the live job's narrowed sandbox
when a plan is open and **the floor** otherwise, so every place that clears the
narrowing means the floor without being edited. The floor is
`Sandbox::read_only`, which downgrades the *explicit* roots in place — the
implicit ones (`SYSTEM_ROOTS` and `/dev/null`) exist so a subprocess can start
and have no write bit to take — and stamps `Authority::Draft`, so a refusal says
the floor refused rather than sending someone to edit `luu.toml`. `chat` keeps
the policy file: a floor is the gate's shadow, and `chat` has no gate.

**The refusal names the draft, one turn in.** `Authority::Draft` carries an
`Option<JobId>`: `None` on the first prompt of a session, where the sandbox is
chosen before `open_draft` runs and there is genuinely nothing to name, and the
draft's id on every prompt after it — stamped by `serve` from the context it is
already reading, and only where the live job really is a draft. So a denial
reads *the floor for draft 3, which grants no writes*, the way one inside an
approved job has always read *the approved plan for job 7*, and a reader stops
having to join the refusal's turn to the line that opened its job.

**And the gate shows it.** The panel had always shown what a plan *asks for* —
reads, writes, commands, network, egress, enforcement — and never what the turn
in front of it may already do, which made approving read as a yes to a list
rather than a decision about what to **add**. `/api/settings` carries the
session's floor beside its posture (`reads`, `commands`, `network`, resolved in
Rust and never re-derived in the page, because a second resolution of a policy
is a second sandbox), and the gate prints it under the asks: *unapproved, a turn
reads `.` · runs `rg`, `ls` — and writes nothing*. There is no `writes` field on
the wire: the floor grants none by construction, so it is a sentence rather than
an always-empty list. See
[`RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`](RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md).

It is only as strong as `enforcement`: under `best-effort` there is no Landlock,
so a subprocess started by a draft turn can write anyway — true of every path
grant here, and worth saying because the floor is the first one whose entire
content is a subtraction. The same change found that **a session resumed inside
an approved job was coming back held by the policy file rather than by the plan
it was approved with** — a silent widening, item 9's shape one field along,
which the resume route now re-narrows. See
[`RECORD/2026-09-21.the-drafts-floor.completed.md`](RECORD/2026-09-21.the-drafts-floor.completed.md).

**The floor and the plan narrow what a turn may *do*; `[authority]` is what a
session may tell it about that, in words this machine chooses.** Built,
off by default, and reachable only from `config.toml` and the page — never
from a flag, and never from `chat`: `Authority::Draft` has no floor to narrow
there, since every job a script opens is pre-approved by the script itself,
so there is no draft state a note would ever attach to. Two tables,
`[authority.draft]` and `[authority.plan]`, each an optional `text` and a
`position` (`system`, the default, or `prompt`) — two and not one, because a
draft and a plan grant different things and the sentence that fits one does
not fit the other; asked and rejected as a single note while this was being
argued. `system` appends the words to the cached prefix once, so they ride it
for as long as the authority holds and cost nothing per turn; `prompt`
prepends them to the turn's own text instead, repeated every time, which is
the one worth having if the question is whether proximity to the turn a model
is about to make changes anything on its own. `Authority::Policy` gets no
table: it is the ordinary grant, and there is nothing about it worth saying
that the tools already in front of a model would not show. **Simpler than
`[resend]`'s session-fact story in one way that matters**: a note is
recomputed from nothing on every `select`, so there is no window of eviction
or pruning state a mid-session move could leave stranded, and `PUT
/api/authority` applies whatever it is asked outright rather than deciding
what is safe to move a live session onto. The wire says what a turn was told
the same way it says which of the three window rules rendered it
(`record::FORMAT` 18, `authority_draft`/`authority_plan` on the header,
`None` on every stream written before this field existed and not *no note
set*). It exists so that the words and the position can be edited by hand
between runs and compared, which is the run
[roadmap row 27](ROADMAP/2026-09-21/README.md)'s claim (c) was blocked on, and
that run has been made once: one seed, one one-line bug, `position = "system"`,
`qwen3.8-27b`. Without a note, a drafting model tried two denied writes before
stating the fix in prose, 2 824 completion tokens; told that writes wait for a
plan, it read once and proposed one, 517. Neither plan was approved, so what the
note does to the work itself is unmeasured. See
[`RECORD/2026-09-22.an-authority-a-model-is-told.completed.md`](RECORD/2026-09-22.an-authority-a-model-is-told.completed.md).

Approval is per piece of work, so there is no autonomy setting to remember and no
mode that can be left open. `plan`/`run`/`auto` used to live here; they were
global where approval is per piece of work, and — the reason they went — a mode
cannot tell the context manager what to compact. See
[`RECORD/2026-08-27.tasks-instead-of-modes.completed.md`](RECORD/2026-08-27.tasks-instead-of-modes.completed.md)
and [`RECORD/2026-09-04.from-tasks-to-jobs.completed.md`](RECORD/2026-09-04.from-tasks-to-jobs.completed.md).

One boundary does three jobs, which is the argument for it:

- **Compaction gets a cut point the work chose** rather than one the counter fell
  on. A closed task is the only rewrite in an otherwise write-once session: plan,
  turns and summary are each written once and never edited.
- **Permission gets its scope**: the approved plan named the files and commands, so
  it *is* the `SandboxPolicy` for that task. One informed approval beats a prompt
  per tool call, which is what trains people to click yes without reading.
- **The transcript gets its grouping**: a closed task collapses to its summary in the
  UI — the view collapses exactly what the context collapses.

A task can still overflow the window on its own, so the boundary is the *preferred*
cut point and the token threshold remains the fallback.

**What exists**: the type, the lifecycle and the fold. A turn carries the task it
was asked inside; closing one is an *event* — its turns stay in the history and
stop being rendered, replaced by a deterministic summary of the approved plan,
what the tool results reported, and the fragments the task's turns were shown —
quoted from the file's own bytes, newest first, under a per-summary token cap,
with no model prose in it. The quote is there because the grounded probe measured
its absence losing answers: see
[`RECORD/2026-08-30.the-fold-probe-run.completed.md`](RECORD/2026-08-30.the-fold-probe-run.completed.md)
for the measurement and
[`RECORD/2026-08-30.what-a-summary-should-carry.completed.md`](RECORD/2026-08-30.what-a-summary-should-carry.completed.md)
for why verbatim rather than a digest. Reopening is therefore
not an undo: the fold stops applying. Eviction runs over items rather than turns,
so a folded task is kept or dropped whole and the two ways history gives way
compose. The budget gained a `summaries` bucket, because a fold nobody can see in
the panel is a claim rather than a measurement.

**How a task is approved**, in the two places there is something to approve:

- **In `luu serve`**, by a person. A prompt arriving with no task open buys one
  planning call — the agent is asked what it is about to do, and answers with a
  fenced ```` ```plan ```` block, parsed the same way a tool call is. The prompt is
  then **held on the server, unrun**, until someone approves or refuses it: no
  turn, no tool, no model call happens behind the gate. A model that answers in
  prose instead does not cost the gate — the proposal becomes the ask itself,
  declaring nothing, and the panel says so.
- **In a script**, which is where a repeatable run comes from. `## task:` names
  the objective, `## step:` / `## file:` / `## command:` give the plan, `## close`
  closes it. The written plan *is* the approval.

Either way approving runs a real check: every file the plan names must be
reachable in the sandbox and every command allowed by it. **And the approved plan
then becomes the sandbox for its task**: every turn inside it is checked against
what the plan named, not against everything `luu.toml` grants, so the task
boundary is the scope permission is granted at rather than a sentence saying it
is. The policy file is the outer bound and the plan the inner one — a plan
cannot grant what the file does not, and a plan that names nothing grants
nothing. A denial says which of the two refused.

Narrowing cuts the level as well as the extent. A plan has two path lists:
`files` is what the task may **read** and `writes` what it may also **change**,
so a file declared only as read is read-only inside the task even where the
policy grants both — and a plan that says it will change a file under a
read-only root is refused *at the gate* rather than four turns in, which is the
one case the check exists for and used to miss. A write to a path that does not
exist yet grants the directory that will hold it, because creating a file is a
write to its directory and at the kernel rung a grant is a directory anyway.
Approving carries an amendment — the reads, writes and commands the person adds
at the gate, checked against the policy file exactly as the model's plan was —
which is what stops an under-specified plan from being a dead run. It carries one
more thing the plan never had: `closes_on`, below, checked against the plan as it
will *be* rather than against the amendment alone, since the command it names is
usually one the model already declared. **And it carries the three that say what
the job may reach**: `network`, `egress` and `enforcement`, each bounded the same
way. Those had been on the protocol since 2026-09-06 and reachable only from a
script or a client the person did not have — the debug UI's approve button sent
four fields and the panel displayed three, so a plan declaring `"network": true`
was signed off by somebody who was never shown it had asked. The gate panel now
shows what a plan asks to reach (and says when the session's own policy denies
it, because what a plan asks for is not what `Plan::narrow` will give it) and
sends all six. **The refusal that comes back survives the turn it describes**:
`not_granted` is the one refusal that does not stop anything — the approval went
through without what the policy would not carry — and the turn it approved
starts in the same breath, which used to erase it within a frame. It is the only
place a person learns the policy file has a floor, so it now stays until the next
gate opens. See
[`RECORD/2026-09-17.the-gate-panel-narrows.completed.md`](RECORD/2026-09-17.the-gate-panel-narrows.completed.md).

**A plan also decides how hard the kernel is asked to hold its children, and it
is the one field that narrows backwards.** `enforcement` (`## enforcement:` in a
script, `enforcement` on `approve_job`, `None` by default and meaning *the
session's*) may be **tightened** by a plan and never loosened: everything else a
plan declares is a grant, where less is narrower and a plan that names nothing
gets nothing, while this is a strictness whose permissive value is
`best-effort` — the value a person types to get `run_command` at all where
Landlock is missing. So a plan asking for `kernel` needs no permission and a plan
asking for `best-effort` inside a `kernel` session is refused at the gate, like
any other thing the policy file does not grant. `Plan::narrow` takes the
strictest of the two, and the denial that follows names *which* authority is
holding the child, because "grant it anyway with enforcement = best-effort" sends
a person to edit a file that already says that. See
[`RECORD/2026-09-06.enforcement-per-job.completed.md`](RECORD/2026-09-06.enforcement-per-job.completed.md).
See [`RECORD/2026-08-30.the-gate.completed.md`](RECORD/2026-08-30.the-gate.completed.md) and
[`RECORD/2026-08-30.narrowing.completed.md`](RECORD/2026-08-30.narrowing.completed.md).

A refusal is kept, not erased: a declined plan stays in `Context::rounds` beside
the one that was eventually approved. Nothing in a session is deleted — a closed
job is folded, a reopened one unfolded, a refused plan recorded. The lifecycle is
a state machine and the messages that drive it come off a socket, so every
transition is guarded: only an open job closes, only a closed one reopens, and
**only the last one** — a reopen behind the front would give the session two open
jobs and the next prompt a choice of two plans to run under. Closing the draft a
plan is offered inside is refused for the reason closing a proposal used to be:
it would take the gate off the screen with the question still up.

**A proposal says who wrote it.** `plan_proposed` carries `source` — `model` when
a drafting turn's answer carried a parseable plan block or the planning call
emitted one, `prose` when it answered without one, `written` for a script's
directives. The gate's headline number is
how often a small model plans at all, and emptiness cannot answer it: a model
that ignored the format and one that declared empty lists arrive as the same
empty plan, and the two want different fixes (a grammar, or a sentence). The read
side keeps the plan as proposed beside the plan as approved, because the
difference between them is what a person had to add — the cost of the gate. How
to measure it is [`RECORD/2026-08-31.the-gate-probe.completed.md`](RECORD/2026-08-31.the-gate-probe.completed.md).

**And the server says no out loud.** A `refused` message carries the request it
answers, a reason (`busy`, `pending`, `task`, `not_granted`) and a sentence for
a person. Before it, a client could not tell a refusal from a message that never
arrived, and the debug UI had to guess by disabling its own composer — which is
not a permission model and not an interface either.

**Who declares a task done** is a ladder, not a single answer: deterministic checks
(exit codes, tests) first, then a judge, then the user's final question, which is the
only authority. The judge is the same model given a short context of **evidence, not
narrative** — the objective as typed, the diff, the commands and their exit codes —
because a judge fed the model's own account of its work inherits its hallucinations.
It runs in shadow mode until measured: every task the user closes is a label, so the
accuracy figure arrives for free and gates nothing until it exists.

**The exit-code rung exists.** A plan carries `closes_on`, one command line, and
the task folds itself the moment a `run_command` step of its own runs exactly
that line and comes back `exit_code: 0` — matched whole, so `cargo test` is not
`cargo test --no-run`, and on the code rather than on the absence of an error, so
a child killed by `SIGXCPU` is the limit reporting an unfinished command rather
than a success. A plan without one closes only when a person says so, which is
every plan a model writes.

**The model is never asked for `closes_on`; the person at the gate types it.**
`PLANNING` keeps its five keys, so the cached prefix does not move and every run
recorded before it stays comparable. The reason is not economy: *what would
convince me this is finished* is the judgement the gate exists for, and a 7B
asked for its own success criterion restates the plan it was about to run. It is
the first thing the gate can add that the model was never asked to propose.

**Every close says which authority folded it.** `task_closed` carries `by` —
`user` or `exit_code` — because a ladder is only worth climbing if each rung can
be counted: how often the exit code closed a task a person would have left open,
and how often a person closed one it missed. Absent means `user`, which is what
every close before the field existed was. See
[`RECORD/2026-09-02.closing-on-an-exit-code.completed.md`](RECORD/2026-09-02.closing-on-an-exit-code.completed.md) —
including the finding that the rung is **unreachable wherever the kernel cannot
hold a child**, macOS included, because it sits on top of `run_command`. The
container is what makes it real.

## Overall architecture

```
┌─────────────┐   ┌──────────────────┐   ┌─────────────────┐
│  CLI (bin)  │   │  VSCode ext (TS) │   │  Web debug UI   │
└──────┬──────┘   └────────┬─────────┘   └────────┬────────┘
       │ in-process        │ stdio               │ WebSocket
       └───────────────────┴─────────────────────┘
                            │   one JSON message schema, N transports
                  ┌─────────▼──────────┐
                  │   agent-core (lib) │  ← all the brains here
                  │  tasks, context,   │
                  │  tools, sandbox    │
                  └─────────┬──────────┘
                            │
                  ┌─────────▼──────────┐
                  │  inference backend  │ (llama.cpp / Ollama)
                  └─────────────────────┘
```

The core knows nothing about CLI, VSCode or the browser — it exposes an internal API (Rust) plus a local server speaking a single JSON message schema over swappable transports. This is also what makes container isolation a packaging question: the container wraps tool execution, not the whole core — see [Container mode](#container-mode) and [`RECORD/2026-09-01.the-container-decided.completed.md`](RECORD/2026-09-01.the-container-decided.completed.md).

## Context management (the differentiating piece)

### The shape of the prompt: one rule

`system`, then strict `user`/`assistant` alternation. Anything that is not a turn —
selected code, a rolling summary — is **fused into the nearest turn's user message**,
never sent as a message of its own. Storage keeps them separate, so pruning can reach
them later without parsing back what was already written.

```
[system]      system text + tool definitions   ← never changes: the cached prefix
[user]        (fused summary) + oldest retained turn
[assistant]   …
[user] …      recent turns, verbatim           ← appended to, never rewritten
[user]        selected code + the new prompt   ← fused, as late as possible
```

What changes most often goes last. Bucket order in the rendered prompt is a cache
decision, not a readability one, and retrieved code never goes in the system block:
it changes every turn, and putting it there invalidates the whole prefix without
anything failing.

### Decide, then render

`Context::select` chooses what fits against the budget and *then* renders; the
rendering is a pure function of the selection, so every token sent is attributable
to a bucket. Rendering everything and trimming the string is the alternative, and it
forecloses the point: a rendered prompt no longer knows where each span came from,
and the cut lands wherever the limit happens to fall — sooner or later inside the
stable prefix.

- **Explicit token budget**: real tokenization with the loaded model's tokenizer
  (HuggingFace `tokenizers` crate, `--tokenizer`), split across system/tools, code
  context, history, and a reserve held back for the answer before any history is
  considered. Without a tokenizer file the count degrades to `chars/4` and is
  **labelled approximate everywhere it appears** — it is not a measurement.
- **Every count carries its counter.** A stored turn records which counter produced
  its tokens, and a budget names the counter that produced it. Two runs measured
  differently are not comparable, and nothing else would say so.
- **Prefix reuse is measured, not assumed.** Every turn after the first carries the
  longest common prefix between its prompt and the previous one, in bytes and in
  tokens. A cache stops at the first difference, so a prefix is the whole quantity —
  matching text after the divergence is reuse the cache never gets. It is measured
  against our own rendering rather than the templated string, which is a proxy and
  labelled as one; it holds because a chat template renders message by message, so an
  identical message prefix is an identical templated prefix. This is the number a
  context strategy is judged by, and it is measurable against the mock backend
  because it is decided before the call.
- **The turn is the unit.** Eviction drops whole turns, oldest first, so the retained
  window can only ever start on a user message. Half a turn leaves an answer to a
  question nobody asked; a window starting on an assistant message makes several chat
  templates continue instead of answering.
- **Order blocks by how often they are rewritten — growing at the end is not being
  rewritten.** A prefix cache reuses the longest common prefix, so a large block that
  changes rarely belongs *above* an append-only history, not below it.
- **Eviction cuts in blocks, not one turn at a time** (`--evict block`, default
  `turn`). Dropping the minimum rewrites the history from its front on every call
  once the window is full, and the reusable prefix collapses to the system block —
  measured at 3–4% from the first eviction onwards, and it never recovers. Cutting
  down to a low-water mark (`--low-water`, 0.5) instead pays for that once every
  four or five turns and holds still in between: mean reuse over the same 20 turns
  goes from 33% to 67%. It costs 21% of the history, which is not free and is not
  yet known to be harmless. Numbers in
  [`RECORD/2026-08-27.prefix-reuse-and-block-eviction.completed.md`](RECORD/2026-08-27.prefix-reuse-and-block-eviction.completed.md).
- **What leaves the window stays out.** Eviction is monotone: `Context` keeps a floor
  that only moves forward. Recomputing the retained window from scratch each turn
  lets a dropped turn return the moment a shorter prompt leaves room, which moves the
  front of the history and is the one thing a prefix cache cannot survive — and it
  makes block eviction impossible, because the next fill walks straight back past the
  cut.
- **And it says so: a cut is a message.** The selection that moves the floor emits
  `evicted` — the turn that cut, the turns that left, what they were worth, the
  counter that counted them and which policy did it. Without it a recording shows the
  history bucket shrinking and cannot say whether that was the policy or the
  arithmetic, which is how a set of degenerate fixtures survived a whole commit. The
  turns are named rather than counted, because a reader months later cannot recover
  *which* without re-implementing the floor. See
  [`RECORD/2026-08-31.eviction-tombstones.completed.md`](RECORD/2026-08-31.eviction-tombstones.completed.md).
- **An unknown window is not an unlimited one**: `--context-limit 0` means unknown, so
  nothing is budgeted and nothing is evicted, and the panel says so rather than
  drawing a bar against nothing.
- **Stable prompt cache / prefix**: keep the fixed part (system + tool definitions)
  byte-identical across calls to take advantage of llama.cpp's prompt caching / KV
  cache reuse.

### The overhead we cannot see

Talking to Ollama's `/api/chat` — or to any OpenAI-compatible `/chat/completions`
— means the chat template is applied on its side: the `<|im_start|>` markers and
per-message separators are tokens we never count. Owning that number would mean
owning the prompt string (`/api/generate` with `raw: true`, or the completions
endpoint rather than the chat one), which is deliberately not where this sits
today.

So the gap is **accepted and reported**: the trace carries our count per bucket,
measured before the call, and the backend's own `usage.prompt_tokens` afterwards, and
the panel shows the difference as its own labelled quantity. A stable difference is
template overhead; a moving one means the template changed, which is worth more than
the precision given up.

For the difference to mean that, the two sides have to count the same calls. A
turn that uses a tool is several model calls — the first ends in a tool block,
the next reads the result — and `usage.prompt_tokens` is summed over all of them.
So the agent loop announces every call it makes before making it, and the trace
measures the ones after the first into the same chain as the turns. Measure only
the first and a 2-token template gap reads as 1 962, with nothing failing and
nothing saying so; see [`RECORD/2026-08-27.the-m4-pro-run.completed.md`](RECORD/2026-08-27.the-m4-pro-run.completed.md).

### Still ahead

- **Hierarchical compaction**: built, and measured once. A closed task is replaced by its deterministic summary — the approved plan plus the evidence: paths, commands and exit codes — and full text is kept only for the live one. The token threshold stays as the fallback for a task that overflows alone. Against the recorded baseline (twenty prompts, 1024-token window, mock backend): mean prefix reuse 69.9% under per-turn eviction and 89.9% under block eviction becomes **91.3%** with four task boundaries, and the prompts sent shrink from 16 715 to **13 417 tokens** — with eviction never firing at all, because the folded history never grew enough to need it. What the fold *loses* is the conversation itself — the turns' own prose — and, until it quoted them, the files the turns were shown: that second loss was measured against a real model (two of three closed-task questions came back wrong, one of them a refusal) and closed by quoting the fragments verbatim into the summary. On the mock pair that quote costs 16 927 → 24 356 tokens over the twenty prompts, against 30 682 unfolded, and leaves reuse where it was (89.6% → 89.3%). On the real model (`qwen2.5-coder:7b`, sampling pinned) both closed-task questions that broke before the fix now answer correctly, matching the unfolded run almost word for word, and the fold still wins on tokens but by less than estimated: 29 135 tokens against 36 288 unfolded, a 19.7% win rather than the ~50% win before the fix paid for the quote — see [`RECORD/2026-08-30.the-fold-fix-verified.completed.md`](RECORD/2026-08-30.the-fold-fix-verified.completed.md). Model prose in the summary stays rejected — it would enter the write-once region every later turn is built on, which is exactly why the quote is the file's bytes and not a digest of them.
- **Grounding a turn with a real file**: built. `--fragment PATH[:START-END]`, and `## fragment:` in a script, read a file **through the sandbox** and fuse it into one turn's user message — the `code` bucket, which existed from the start and until now was always zero. A path the sandbox would refuse to `read_file` is refused here too, and a denial is an error rather than a warning: a run that quietly dropped its grounding answers out of the model's training and looks like it worked, which is exactly what a real 7B did to twenty ungrounded turns. It is attached to one turn and then gone, because which turns a file belongs in is the next item's question and attaching it to all of them answers it wrongly, at every turn's expense. `scripts/tasks/grounded{,-tasks}.txt` are the pair this makes possible: the same twenty prompts, one grouped into tasks, with a last group that attaches nothing and asks the first fifteen again — the first corpus in which *does the fold lose what the task needed* is a question with an answer. The protocol for asking it is [`RECORD/2026-08-27.grounded-fold-probe.completed.md`](RECORD/2026-08-27.grounded-fold-probe.completed.md). `scripts/tasks/edit-reread.txt` is the one that **edits** what it grounds — the same directive over a committed scratch tree rather than over this repository, because every prompt that changes a file changes it for the rest of the session — and it is what makes *is this file's content in this prompt twice* a question with a number instead of an argument.
- **The repository map**: built, and ranked **behind a flag that is off**. Every `.rs` file's definitions with their signatures, bodies elided, from `tree-sitter`'s own `TAGS_QUERY`, in the cached prefix under the tool definitions — where blocks are ordered by how often they are rewritten, and the map changes only when the repository does. `--map-tokens N`, off by default so that every recording made before it stays comparable, and `luu map` prints the exact bytes. Files are outlined in path order until one does not fit; the map then says how many it left out. **`--map-rank` orders them by what the rest of the tree depends on instead** — a reference graph over the same query's `@reference.*` captures, PageRank with a *uniform* teleport, which is what keeps the map a prefix: Aider seeds its ranking with the files in the conversation, and that is the half that would rewrite the block every turn. It is off because it was measured and lost, twice, on two differently-built corpora: at 1024 tokens path order holds five files and the ranking holds two, because rank order puts the big central files first and the fill rule stops at the first file that does not fit — and on a corpus of 38 questions chosen one per file before either order was checked (so the loss cannot be the corpus favouring path order's own holdings), path order named the right file for **100%** of the questions it covered against rank order's **12.5%**, and rank order's denser, more self-referential files pushed a 7B into paragraphs of fabricated Rust that evicted the session's own history 24 times in 38 turns, where path order evicted nothing. `luu map --explain` prints the ranking under either order — each file's score and who references it — so the order the map did not take can be read beside the one it did. To address the oversized file trap and leaf-sink penalty, **`--map-in-degree`** weights inbound references by caller diversity rather than random-walk damping, and **`--map-non-greedy`** skips over files that exceed the remaining token budget to continue packing smaller ones (packing 5 files and 1026/1024 tokens at 1024 tokens, vs 2 files and 664 tokens under greedy fill). This repository is still the argument for doing this properly: its whole outline is **6 327 tokens, 77% of an 8K window**, so at any affordable budget most of it is missing and the alphabet chose which part. What it costs on the grounded script: 870 tokens a turn at `--map-tokens 1024`, +56% on the run's total prompt tokens — and a prefix-reuse number that rises from 93.9% to 96.3% for purely arithmetic reasons, which is why a map-on run is not comparable on reuse to a map-off one. See [`RECORD/2026-08-31.the-repo-map.completed.md`](RECORD/2026-08-31.the-repo-map.completed.md), [`RECORD/2026-09-02.ranking-the-map.completed.md`](RECORD/2026-09-02.ranking-the-map.completed.md), [`RECORD/2026-09-03.the-map-order-probe.completed.md`](RECORD/2026-09-03.the-map-order-probe.completed.md) and [`RECORD/2026-09-04.in-degree-and-fill.completed.md`](RECORD/2026-09-04.in-degree-and-fill.completed.md).
- **Relevance over recency**: built, measured, and **on this repository's own corpus it is the largest single win the context manager has**. `--select-tokens N` fills the `code` bucket with the definitions *this turn's text points at*, chosen by `agent_core::select` and read through the sandbox by the same loader `--fragment` uses — so a selected fragment enters a turn by the door that already existed, and is gone when the turn is. **The mechanism is `tree-sitter` tags plus what the file says it is for, and the reference graph is off**: on the 38-question corpus, at 1024 tokens, a path-ordered map holds the answer to **3** of the questions and a selection holds **33**, with the right file first 19 times and in the top three 31. The graph hop — one step from a file that matched, which is what this module was expected to be *about* — was implemented, measured, and **lost a third time**: it moves the right file out of first place five times in 38, holds no more targets, and costs 13% more tokens, because on a tree this connected a neighbour is most of the tree. It ships off, switchable, exactly as `--map-rank` did. The doc signal is doing much of the work and the corpus was written from module docs, so the ablation is the honest row: tags and path alone hold **19** of 38 at 1024, still six times the map. What is *not* measured is precision — whether a model uses what it was handed — and that needs a box. Off by default, because it changes every number in every recording made before it, and unlike the map it is **not** cached: a run with selection on is not comparable on prefix reuse to one without. Decided against Aider's implementation and against embeddings; see [`RECORD/2026-08-27.aider-repo-map.completed.md`](RECORD/2026-08-27.aider-repo-map.completed.md) and [`RECORD/2026-09-05.choosing-fragments.completed.md`](RECORD/2026-09-05.choosing-fragments.completed.md). **Precision is now measured too, and the selector is used**: the same 38 questions against `qwen2.5-coder:7b`, one flag apart, answer **33** correctly with selection on and **0** with it off — handed nothing the model does not know the tree is Rust and answers `main.py`. Precision on the 33 targets it held is **91%**, there is no stuck default, and *position inside the bucket does not matter*: held-but-not-first scores 13/14 against first place's 17/19, which retires "it moves the right file out of first place" as a reason to reject anything. The price is now known and it is not the per-turn prompt: a fragment is fused into the turn's user message and **stays in history**, so a 38-turn run evicted 31 times, prefix reuse fell from 93.3% to **17.7%**, and the run took **11.6×** the wall clock of the ungrounded one. Dropping the quoted code from a turn once it is answered is untried and is the same problem as active pruning below. See [`RECORD/2026-09-06.does-the-model-read-it.completed.md`](RECORD/2026-09-06.does-the-model-read-it.completed.md), whose answers are on disk in [`RECORD/runs/2026-09-06.precision/`](RECORD/runs/2026-09-06.precision/). **It is in every surface, not only `chat`**: `serve` and `stdio` take the same flag, the tree is walked at startup beside the map and **re-stamped before every turn** — `rewalk_sources` re-parses only the files whose mtime or length moved — and a turn selects under the lock that hands out its number. The re-stamp is not an optimisation of the walk, it is a correctness fix: a walk kept from startup does not stop offering a file the session edited, it offers it at the line numbers it had before the edit, and the fragment loader reads the *current* file at them — wrong lines under a right path. It refreshes through the **agency's** sandbox rather than the live job's, because the walk is the session's cache and a narrow plan must not shrink it for every turn after; the narrowing stays at the read. The map is deliberately not rebuilt: it is the cached prefix, and a stale line number a reader follows by hand is not the same failure as a stale range delivered as the definition. In those two the selection is read through the **live job's** sandbox rather than the policy file — an approved plan narrows what may be *chosen* exactly as it narrows what may be opened, which is what makes a selector admissible behind a gate at all. The consequence is that a plan a 7B wrote bounds the selector's reach: on the end-to-end run the planning call selected over the whole tree at 2 678 prompt tokens, the model's plan granted one file, and the approved turn selected at 797. A person who knows better adds files at the gate, which is what `approve_job` already carries. See [`RECORD/2026-09-06.selection-at-the-gate.completed.md`](RECORD/2026-09-06.selection-at-the-gate.completed.md).
- **What leaves the history**: **built, three rules, one of them on by default, and each one a session's to choose** — `POST /api/sessions` takes them beside the provider and the posture, so what the paragraph below calls a default is what a session starts on and no longer what a process is stuck with. **Rule A is on and B and C are off**, which is not the three ranked by what they save: ranked that way the winner is C, the only one of the three that has never been put in front of a model. The criterion is *fewest tokens without affecting the result*, and it selects the one rule whose saving has been shown to cost nothing — n=1, and said so where it is decided. A `cat` capped at 8 KiB and a selected fragment are the same object to the window — a block of file bytes that earned its place in *one* turn and is charged to every turn after it — and by turn 20 of the precision run ~90% of the `history` block is quoted code. Three rules over one window, and they are not the same size: two of them reach quoted code and the third reaches the block the first two were named after and could not touch. **Rule A, *do not re-inject*, is built and on by default since 2026-09-19** (`--repeat-once`, `--no-repeat-once`, `Budget::repeat`): a span is rendered by the **oldest** turn of the window that carries it and skipped by every turn after it, the turn being asked included. Oldest and not newest is the whole reason it is cheap — the block above the newest message stays byte-identical, so the only thing that moves is the message that moves anyway — and it is also what stops eviction orphaning a span, because ownership is only ever handed *forward*, to a turn that will outlive the one giving it up. The owner is chosen inside the window being rendered and never over the session: an owner already below the floor is [`the-fold-fix-verified`](RECORD/2026-08-30.the-fold-fix-verified.completed.md)'s failure with a different mechanism, and the precision corpus offers it 22 chances. A fragment's identity is its **path and its bytes**, so the same span after an edit is a different span — which is the right rule for *has this exact block already been rendered* and the wrong one for *is this file's content in this prompt twice*. **So the promise needs a second half, and since 2026-09-20 it has one: the span that is rendered once is the *newest* one.** `code_context` is written when a turn closes and never refreshed, while every turn re-reads its own spans, so a file edited mid-session used to go out as two blocks under one `// path` header with nothing saying which was true — and rule A could not collapse them, because by its own identity rule they are different spans. **Now the newest body of a path wins and the older ones go out as the line that cites them** (`// path — N tokens, superseded`), where *newest* means the body carried by the newest turn that read it, the turn being asked ahead of all of them. **Derived and not stored, which is what makes it a ratchet**: it is computed over every turn the session holds, the evicted ones included, so the newest reader of a path only ever moves forward and a body the session watched go stale cannot come back by anything *leaving* the window — the floor and the prune line each needed a field for that property and this one gets it from the derivation, on `SessionView::counts()`'s argument one structure along. A file edited **back** to what it was is the case that decides the shape: a ratchet over bodies ever found stale would go on citing bytes that are on disk, and a ratchet over readers does not. The citation is **unguarded**, which is the one place it parts company with rule B's: a pruned span's citation is a *saving* and is not taken where it costs more than the span, and this one is a *correction* — what it replaces is not expensive, it is untrue, and being cheap to send is not a reason to send bytes the session knows are stale. It is **rule A's repair and not a fourth rule**: nothing is superseded under `Repeat::Always`, where a prompt carrying one path twice is the documented behaviour and the blocks read as per-turn snapshots, so there is no `--no-supersede` — the way back is `--no-repeat-once`, and a run made without rule A is byte-identical to what it was. The detector stays exactly as it is, unchanged and still computed over what was sent: `Selection::diverged` and the `diverged` **trace** line report every path a render sent under more than one body, and the only evidence the fix works is that they now find nothing. Beside them, `Selection::superseded` and a `superseded` trace line say what the render did instead (`record::FORMAT` 15; `protocol::VERSION` unchanged at 6) — trace for the same reason `pruned` is one, since every turn is still asked, answered and in the transcript and what changed is the prompt. **That bump makes an older line mean something new, which none of the others did**: a stream at 12 to 14 carries `diverged` because the defect was reported and not repaired, and a stream at 15 carries `superseded` where those would have been, so `diverged` empty is *nothing went stale* on an old stream and *everything stale was replaced* on a new one, and the format number is what tells them apart. The measurement that decided it, on [`scripts/tasks/edit-reread.txt`](scripts/tasks/edit-reread.txt) — thirteen turns over a committed scratch tree that reads four files, edits three and asks about two of those again, with a control that must stay quiet and both blind spots in it — is **two edits leaving 8 of 13 prompts carrying two bodies for one path**, six of the eight attaching nothing of that file, so a divergence is a state the window enters rather than an event at the turn that causes it. Against the parent commit, same corpus and same flags: **8 of 13 renders repaired, 10 spans, 898 tokens of stale bytes replaced**, session prefix reuse 93.95% → 87.14% with **every point of that in the two turns that re-read the edited file** — the break is one per *edit* and not one per render, because the citation standing in the old block's place is itself stable — and the history bucket **falling** 7 200 → 6 432 tokens, since a citation is cheaper than the bytes it replaces. The repair is not paid for out of the window; it pays the window back. What it does **not** do is settle whether a model notices: the edits are scripted, so the number is a fact about the corpus, and what a *model* does is the run the corpus exists for. Two blind spots are unchanged and inherited exactly, because the fix reads the same identity the detector does: a file edited after its only read has nothing to contradict it, and two *overlapping* specs of one file (`1-6` and `1-16`) are two paths. See [`RECORD/2026-09-19.one-path-two-bodies.completed.md`](RECORD/2026-09-19.one-path-two-bodies.completed.md), [`RECORD/2026-09-19.a-corpus-that-edits.completed.md`](RECORD/2026-09-19.a-corpus-that-edits.completed.md) and [`RECORD/2026-09-20.the-newest-body-wins.completed.md`](RECORD/2026-09-20.the-newest-body-wins.completed.md), with the run in [`RECORD/runs/2026-09-20.the-newest-body-wins/`](RECORD/runs/2026-09-20.the-newest-body-wins/). On twenty grounded turns at `--select-tokens 1024`, one flag apart: the `code` bucket falls **19.5%** (20 274 → 16 318 tokens), the history does not grow to absorb it, the window fills slowly enough to need **8 cuts instead of 12**, and mean prefix reuse goes from 34.0% to **52.0%** — that last one predicted by nothing and a consequence rather than a claim. The accounting had to follow or the budget would lie: a turn costs its stored count *less what this render drops*, and `Evicted.tokens` is now the difference of two windows rather than the sum of the items below the floor, which was an over-report the moment a span could outlive the turn that owned it. **And since `record::FORMAT` 14 the rule says what it did**: a `repeated` trace line names the turns that gave up spans, how many, and what they would have cost — trace for the reason `pruned` and `diverged` are. Until it, rule A was the only one of the three that reported nothing at all: the number was computed in `split_shown`, subtracted from the `history` bucket and dropped, so a run that collapsed forty spans and one that collapsed none left the same evidence, which is the complaint format 11 was bumped to fix one field along. It is **the sum of the spans a render did not resend** and deliberately not what `Pruned.tokens` is — that one is the difference of two windows precisely because a span its turn owned passes to a younger turn instead of leaving, and this is the other side of the same passing-down, so it is not what the window would have been under `Repeat::Always`. On the edit-and-re-read corpus it is 7 renders and 854 tokens with fragments typed by hand, and **11 renders, 43 spans and 2 466 tokens with a selector on**, which is where the rule's own justification comes from showing up as an arm rather than as a figure. See [`RECORD/2026-09-19.one-counting-surface.completed.md`](RECORD/2026-09-19.one-counting-surface.completed.md) and [`RECORD/runs/2026-09-19.one-counting-surface/`](RECORD/runs/2026-09-19.one-counting-surface/). **Rule B, *prune behind*, is built and off by default** (`--prune-behind`, `Budget::prune`): under pressure the oldest turns of the window give up their spans and keep their exchange — the code becomes the line that cites it (`// path:1-15 — 312 tokens, pruned`), and a turn is evicted whole only when pruning the whole window is still not enough. The prune line is a **second ratchet beside the floor**: it only moves forward, so between two moves the pruned turn's render is byte-identical, and it moves to the **eviction policy's own target**, so depth against frequency stays one trade with one knob rather than a second invented constant. A citation that would cost more than the span it replaces is not taken. It is not monotone under rule A — pruning the turn that *owns* a shared span hands the whole span to a younger turn and adds a citation where it stood, which is bigger — so the line moves to the shallowest position that fits and otherwise to the one that leaves the window smallest, which means **rule B declines where rule A has already taken the saving**. What it buys is not tokens: on the same twenty grounded turns the prompt moves −0.3% and **nothing is evicted at all** — 0 turns dropped against 13, with 13 732 tokens of quoted code out of the window and the whole conversation still in it. Under `--evict block` the line moves **4 times instead of 13** and mean prefix reuse is 69.8% against the 66.5% of the arm that was dropping turns, which is the policy's character being inherited rather than re-invented. A run under the flag says so on the wire: a `pruned` **trace** line names the turns that gave up spans and what the window saved — trace and not protocol, because a pruned turn is still asked, still answered and still in the transcript, so what changed is the prompt and not the conversation (`record::FORMAT` 8, `protocol::VERSION` unchanged at 5). That line says when the rule **fired**, which is not the same fact as which arm the run was: a `--prune-behind` run that pruned nothing was indistinguishable from a run without the flag until the header carried all three rules itself (`record::FORMAT` 11). A resumed session **used to** have nothing to prune, because `Context::from_view` rebuilt turns without their fragments. Since 2026-09-19 it rebuilds them **by reference** — a `grounded` protocol line carries each turn's specs and `from_view` reads them again through the sandbox the resume is under — so a resumed session has spans to prune like any other, and the one it cannot read comes back as this same citation with the reason in it. See [`RECORD/2026-09-19.fragments-by-reference.completed.md`](RECORD/2026-09-19.fragments-by-reference.completed.md). **Rule C, *a result is bytes too*, is built, off by default, and cuts by tool** (`--prune-results`, `Budget::results`): the 8 KiB cap means one `cat` is ~2 000 tokens at the approximate counter — twice what a whole turn selects at `--select-tokens 1024` — charged to every turn after the one that made the call, and until it landed the only thing that ever took it out of the prompt was evicting that turn with its question and its answer. It has no line of its own: rule B already decides which turns are old enough to give something up, and this decides whether tool output is one of the things they give, which is why it is **inert without `--prune-behind`** and why it is a third flag rather than a widening of the second — there is a run on disk under `--prune-behind`, and a flag that quietly starts meaning something else turns a recorded arm into an unrecorded one. What is replaced is the result *whole*, its `[name] ok` head included, so the citation reports the cost of exactly what is no longer there (`[run_command] output — 1 987 tokens, pruned`); the assistant message that asked for it is never rewritten, because it is the model's own words and it is what says which call the line answers. The same guard as a span's citation: one that is not cheaper than what it replaces is not taken. It reaches a case rule B cannot — a turn that ran a tool and selected nothing has no spans to give. It was also the only thing that reached a *resumed* turn at all, while fragments did not survive a resume; now that they do, that is one case narrower and the rule is unchanged. The fold is unaffected either way: `summary_text` builds its evidence from the stored steps, not from what was rendered. What it does **not** have is a number: no run has been made one flag apart, on this or on any corpus. The 8 KiB cap stays what it always was. **And it is three values rather than two**, because tool output is not one kind of thing: `cited_reads` gives up what could be asked for again — a file's bytes, a directory listing, the same object as a selected span by this project's own account — and keeps what a command found, which is a *record of something that happened* rather than a view of something still there. That is where the measured saving actually came from: every one of the 20 turns that freed 32 752 tokens called `read_file` on a 136 KB file against the 8 KiB cap, and **no corpus here has ever pruned a command's output**, so `cited_reads` ships the whole number and makes none of the claim item 5 is open about. `cited` still takes both and is unchanged — there is a run on disk under it, and a value that quietly starts meaning something narrower turns a recorded arm into an unrecorded one, which is the sentence rule C's own flag exists because of. `--prune-results` still means `cited` for that reason; `cited_reads` is reachable from `[resend]` and from the page and not from the flag. An **unknown tool's output is evidence**: a sixth tool arriving as cheap-to-lose would be the same failure one level down, so the registry's size is asserted in a test and a new tool has to say which it is. The render and the accounting go through **one** function, because under two values each site could carry its own `match` and stay honest, and under three a second copy would be a window whose reported cost and actual cost differ by exactly one tool's output. See [`RECORD/2026-09-07.a-span-is-rendered-once.completed.md`](RECORD/2026-09-07.a-span-is-rendered-once.completed.md), [`RECORD/2026-09-08.prune-behind.completed.md`](RECORD/2026-09-08.prune-behind.completed.md) and [`RECORD/2026-09-09.what-a-result-costs.completed.md`](RECORD/2026-09-09.what-a-result-costs.completed.md), with the runs in [`RECORD/runs/2026-09-07.repeat-once/`](RECORD/runs/2026-09-07.repeat-once/) and [`RECORD/runs/2026-09-08.prune-behind/`](RECORD/runs/2026-09-08.prune-behind/); the proposal the first two rules come from is [`RECORD/2026-09-06.what-leaves-the-history.completed.md`](RECORD/2026-09-06.what-leaves-the-history.completed.md).

## Tool calling: how actions actually get executed

The model never executes anything directly — it only emits a structured request that the program interprets and executes.

- **Agent loop**, built:
  1. Prompt + history + tools → model
  2. If a tool call: parse → validate against the `Sandbox` → execute (real Rust code) → append the call *and its result* to the turn → back to 1
  3. If plain text: end of turn

  Capped at `--max-tool-steps` (8), and exhausting it ends the turn with
  `EndReason::ToolLimit` rather than presenting an investigation cut short as a
  conclusion.
- **The steps are messages, and they alternate**: a turn renders as `user(prompt)`,
  then `assistant(call) · user(result)` per step, then `assistant(answer)`. The
  fusion rule applied to a new kind of content — and it means a turn with tool
  calls is still *one* turn, evicted whole, so the model never sees a result
  whose call has gone.
- **How the model expresses a call is a transport detail.** `ToolCall` is the type
  and the parser is one function. It reads a fenced ```` ```tool ```` block
  out of plain text, which works against any backend including a 7B that has
  never seen a tool API. **Native function calling is built, per destination,
  and off by default** (`tool-calls = "native"` on a profile, `--tool-calls
  native` for a run, recorded in the header as `record::FORMAT` 19). The tools
  travel in the request's own `tools` field, so the server's chat template
  renders them with **the tool prompt the model was trained on**, and our
  fenced block leaves the system text. The `tools` bucket counts the specs'
  JSON, which is what is sent. Nothing above the backend changed:
  `backend::native` converts the history on the way out (an assistant message
  holding a call, followed by the user message that is its `[name]` result,
  becomes `tool_calls` plus a `tool` message, found from the alternation this
  design already guarantees), and a call the server parsed comes back up as
  the canonical fenced text. The measured gain is the prompt, not the parser.
  `qwen2.5-coder:7b` on `llama-server` called on **15 of 15** probe prompts
  under native against **7 of 15** fenced, one flag apart. It never wrote the
  template's tags: every call arrived as a ```` ```json ```` block in the
  content, which `parse_call` and the page now read as a call. It also called
  a tool nobody asked for, an `edit_file` in `luu chat` where the policy file
  granted writes, which is why the default did not move. See
  [`RECORD/2026-09-30.native-tool-calls.completed.md`](RECORD/2026-09-30.native-tool-calls.completed.md).
  **Constrained decoding is built, off by default, and is two different bets, not
  one.** `--constrain grammar` compiles a GBNF that keeps the "just answer"
  branch reachable and forces the one correct fenced shape to complete exactly
  once it is started — measured against a real `llama-server` (`agent_core::grammar`,
  [`the-alternation-that-was-not-one`](RECORD/2026-09-14.the-alternation-that-was-not-one.completed.md),
  [`what-constrain-does`](RECORD/2026-09-14.what-constrain-does.completed.md)):
  7/15 clean calls against 3/15 unconstrained on the tool-call probe, and the
  fence-continuation failure eliminated outright. `--constrain schema` sends
  `response_format`, forcing *every* reply into a call with no "just answer"
  branch at all — measured to exhaust `--max-tool-steps` on **15 of 15** probe
  prompts, because a model given a result from its own call and still not
  allowed to answer just calls again. Built instead as the retry
  `Tools::call_schema`'s own doc names: `SchemaRetry` sends `schema` only
  once, after an unconstrained first attempt scores `Drifted` — fenced under
  a tool's own name, not a decision not to call — and never on a clean
  decline. Measured against `qwen2.5-coder:7b`: 7/15 unconstrained drifted
  replies recover to 13/15 executed calls
  ([`a-grammar-for-tool-calls`](RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md)).
  A retry the backend refuses outright — a schema or grammar it will not
  compile — falls back to the pre-retry drifted answer instead of failing the
  turn, and reports the refusal once, loudly, on stderr rather than the wire:
  `TurnEvent::ConstraintRefused` is debug data, like `ModelCall`, and never
  becomes a protocol message. `--constrain` reaches `chat`, `serve` and
  `stdio` alike, one flag built once per session rather than compiled per
  turn. `llama-server` honours a bare `grammar` field as an undocumented
  extension; Ollama's own `/v1` and native `/api/chat` do not, and say so once
  per run rather than silently sending nothing (`Backend::constrain_caveat`).
- **The definitions are the second half of the cached prefix**, so their rendering
  is a wire format: tools sorted by name, schemas serialized through
  `serde_json`'s sorted maps, nothing interpolated. `luu tools` prints the exact
  bytes. The budget has a `tools` bucket beside `system` for the same reason —
  "the system block grew" is not an answer to why the window is full.
- **File editing via diff/patch** (not full rewrites): `edit_file(path, old_string, new_string)`
  replaces an exact, *unique* occurrence and refuses an ambiguous one. Saves output
  tokens (the slow part of local generation) and reduces errors; the uniqueness rule
  is the safety half, because a replacement that matched twice would edit the one
  the model was not looking at.
- **Streaming**: the preceding reasoning text streams, but an unclosed block is a
  call still being generated, and half a call is not a call.

The set: `read_file`, `list_dir`, `edit_file`, `write_file`, `run_command`.
Output is capped at 8 KiB with a `truncated` flag, and the cap is the part that
is not a strategy: it is what stops one `cat` blowing the window open at the
moment it arrives, in the turn that needs the bytes. The later change it was
waiting for is built and off by default — `--prune-results`, below: behind the
prune line the output becomes the line that cites it, and the call that asked
for it stays as the model wrote it.

**The outcome is structured and the rendering is not.** A `run_command` outcome
carries `exit_code`, `signal`, `stdout`, `stderr` and `duration_ms` as *fields*,
on the protocol and in the record; `ToolOutcome::render` still produces the same
short plain text it always did, because a 7B pays for every token of a wrapper it
does not read. The distinction is load-bearing rather than tidy: the exit code
used to live for one moment inside the string `"{program} exited with {code}"`,
and **a task cannot be closed on an exit code that was never a field** — the
ladder above the user (exit codes and tests, then a judge in shadow mode) was
blocked on this and on nothing else. The two streams are separate for the same
reason: a judge that has to re-split one blob on `--- stdout` is parsing a
rendering. `signal` is how a run says *which* limit stopped a child — `SIGXCPU`
and `SIGXFSZ` are `[sandbox.limits]` arriving, and nothing else in the outcome
could tell them from a crash. Absent for every in-process tool, where there is no
exit code and a zero would be a lie; additive on the wire, so the record format
did not move. See
[`RECORD/2026-09-01.what-the-audit-left.completed.md`](RECORD/2026-09-01.what-the-audit-left.completed.md).

## Sandbox / security

Three rungs, and the middle one is what makes the first worth having. Built: 1
and 2, and 3 in its development posture. See
[`RECORD/2026-08-27.tools-and-sandbox.completed.md`](RECORD/2026-08-27.tools-and-sandbox.completed.md).

1. **In-process checks, and then the kernel again at the open.** Canonicalize
   (`std::fs::canonicalize`) before comparing, or a symlink walks straight out.
   That check answers *may this be opened*; between it and the `open` the path is
   a string, and a string is not a file — so the tools that open one do it with
   **`openat2(RESOLVE_BENEATH|RESOLVE_NO_MAGICLINKS)` relative to the root that
   granted it**, which makes the kernel refuse an escape *during* resolution
   instead of us refusing it beforehand. Who can exploit the window is not
   hypothetical: `write_file` is granted inside the tree by design, so the model
   can make the symlink itself. The verdict is written before the open and
   **upgraded after it** — `Applied::Kernel { how: "openat2(…)" }` where the
   syscall ran, `Applied::Process` where it is missing (pre-5.6, a seccomp
   profile that blocks it, macOS) and the old two-step is what happened. `..`,
   absolute paths and links out are refused; a link that stays inside the tree is
   ordinary and still works, which is why `RESOLVE_NO_SYMLINKS` was rejected.
   **A root asked for by its own name resolves as its own last component,
   beneath the directory holding it** — the ordinary case at the gate, where a
   plan grants a *file* and the file therefore is the job's root: `.` beneath a
   regular file is `ENOTDIR`, and for three days that meant an approved plan
   could not read the one path it had been approved for. A root is canonical, so
   that component is not a symlink and one non-`..` step cannot leave the
   directory it resolves in. See
   [`RECORD/2026-09-08.the-surfaces-first.completed.md`](RECORD/2026-09-08.the-surfaces-first.completed.md).
   `list_dir` is still the old two-step, because a directory cannot be read from
   a descriptor through any stable API — it leaks names rather than contents, and
   it is named rather than quietly excepted. See
   [`RECORD/2026-09-05.beneath-the-root.completed.md`](RECORD/2026-09-05.beneath-the-root.completed.md).
   A subprocess gets none of this: it makes its own syscalls, which is what
   level 2 is for.
2. **The kernel, same process tree, no image and no daemon.** Landlock for the
   filesystem and seccomp for sockets, applied to the child between `fork` and
   `exec`. `run_command("cargo", …)` otherwise hands a build script the same
   authority the agent has, and checking the string `cargo` against an allowlist
   and calling that a sandbox is the part that would be a lie.
3. **A container.** Below, and *on top of* level 2 rather than instead of it:
   Landlock survives `exec` and cannot be dropped. On a Mac it is not a
   duplicate boundary, it is **the only one there is** — which is why it comes
   before the measurements that need it rather than after them.

**Declarative config (TOML)** per project — `luu.toml`, and `luu tools` prints the
resolved result:

```toml
[sandbox]
enforcement = "kernel"          # or "best-effort"
network = false
commands = ["cargo", "git"]     # program names, never a shell string

[sandbox.limits]                # what a child may spend, not what it may reach
cpu-seconds = 300               # RLIMIT_CPU, per process
file-size-mb = 1024             # RLIMIT_FSIZE, per file
# memory-mb = 4096              # RLIMIT_AS — off by default, see below
# processes = 512               # RLIMIT_NPROC — off by default, see below

[[sandbox.paths]]
path = "."
access = "read-write"           # read | execute | read-write
```

- **Longest match wins**, so a narrower rule under a broader one grants more.
- **There is no deny list**, deliberately, though an earlier version of this file
  had one. Landlock is allow-only: a subtraction could be honoured in-process and
  could not be honoured in a subprocess, so `denied = ["./.env"]` would stop
  `read_file` and would not stop `cat .env`, with nothing in the config saying so.
  The way to deny is to not grant.
- **The commands allowlist is a program name.** `run_command` takes one `argv`
  array, program first, and never a shell string, which would make the
  allowlist meaningless — `sh -c "cargo test; curl …"` passes any check that
  looks at the first word. (Until 2026-09-15 this was a `command` string
  beside an `args` list — the same information, but shaped like a call in its
  own right, which is why a model sometimes emitted it unwrapped instead of
  inside `arguments`; see `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`.)
- **Allowing any command implies read+execute on the system roots** (`/usr`, `/bin`,
  `/sbin`, `/lib`, `/lib64`, `/etc`, `/opt`), because a program cannot run without
  reading its own interpreter. **For the child only** — an in-process tool sees
  only what was written down, or `commands = ["ls"]` would quietly grant the agent
  `/etc`.
- **Permission validation lives in the program's code**, not in the model behaving well.
- **The paths say what a child may reach and `limits` says what it may spend.**
  Nothing said the second thing until the limits existed, and the gap was not
  academic: a fork bomb, a disk bomb and a memory bomb all ran to completion
  inside `run_command`'s 30-second clock, whose only other companion was an 8 KiB
  cap on what got *reported* about them. `setrlimit` is POSIX rather than Linux,
  so the limits sit **above** the `linux.rs`/`fallback.rs` split and are the
  first rung that holds a child on macOS at all — where `how` reads `rlimits (…)`
  with the same `missing` as before, instead of "in-process check only".
  `cpu-seconds` and `file-size-mb` are on by default; `memory-mb` (`RLIMIT_AS`)
  is off because a Rust toolchain reserves address space far above what it
  commits, and `processes` (`RLIMIT_NPROC`) is off because the kernel counts it
  **per real uid, not per process tree** — a default there would deny `fork`
  over processes this agent never started, and the right mechanism for it is a
  pids cgroup with the container. A limit is applied soft and hard together — a
  child that can raise its own soft limit back is not limited — with one
  measured exception: `RLIMIT_CPU` is two-stage by design (soft sends `SIGXCPU`,
  hard sends `SIGKILL`), so equal limits collapse into a bare signal 9 that
  cannot be told from a crash. It gets one second of hard grace, which buys the
  signal that names the limit. See
  [`RECORD/2026-09-01.what-the-audit-left.completed.md`](RECORD/2026-09-01.what-the-audit-left.completed.md).

### Who enforced it is reported, never assumed

Level 2 is Linux-only, so `enforcement` decides what happens where it is not
available — the one place in this design where a security property is a setting:

- `"kernel"` (default) — a subprocess runs only if the kernel took the ruleset and
  the filter. On macOS, or a kernel without Landlock, `run_command` is **denied**,
  and the denial names what is missing and the flag that lowers the bar.
- `"best-effort"` — apply what this kernel has and report the gap.

Either way every verdict carries `Applied` — `Process`, `Kernel { how }`, or
`Partial { how, missing }` — and `how` names every mechanism holding the child,
the rlimits and their numbers included, *and its version*,
because Landlock's older ABIs mediate less. Nothing here may say "sandboxed"
without saying by what: a run whose subprocesses the kernel held and a run whose
subprocesses nothing held are not the same run, and afterwards the recording is
the only thing that could tell them apart.

The same rule now runs one step earlier, at the gate rather than at the call: a
job records **who approved it** — `operator` for a person at the surface,
`key { name }` for a signature that verified — beside who enforced it. An
approval nobody can prove the authorship of was, until signatures existed, the
one authority in the system taken on trust because it arrived on the right
socket.

What level 2 does *not* claim: it is not a network namespace (blocking the
internet address families stops a program opening a connection, not one that
inherited a socket). Canonicalize-then-open **no longer** leaves a TOCTOU window
for the in-process tools: they open with `openat2(RESOLVE_BENEATH)` relative to
the granting root, so the kernel refuses an escape during resolution and the
verdict reads `Applied::Kernel`. Where the syscall is missing — pre-5.6, a seccomp
profile that blocks it, macOS — the old two-step is what happens and the verdict
says so.

## Container mode

Level 3. **The development posture is built**: `luu-worker` in a long-lived
container, wide open, with the narrowing kept as a separate item whose trigger is
a fact rather than a date. Level 2 stays applied inside it — inside a Linux
container Landlock works, is free, and is the one part of the sandbox the whole
exercise exists to reach, so the loosening widens `commands` and `network` and
leaves `enforcement` alone. See
[`RECORD/2026-09-01.the-container-decided.completed.md`](RECORD/2026-09-01.the-container-decided.completed.md)
for the decision and
[`RECORD/2026-09-02.the-worker-and-the-seam.completed.md`](RECORD/2026-09-02.the-worker-and-the-seam.completed.md)
for where it cuts in the code.

**The seam is one function.** The tool loop touches the tool set in exactly one
place, so the container arrives as an `Executor` behind it: `Tools` runs a call
here, `Worker` writes it down a pipe. The tool *definitions* stay on the host,
because they are the second half of the cached prefix and a prefix assembled
inside the image is one that moves every time the image is rebuilt. If adding
the container had had to touch the loop, the loop was wrong.

**The container's only process is the worker.** `<runtime> run --rm -i … luu
worker`, spoken to over stdio — the same transport the VSCode extension uses,
pointed the other way. So the container's lifetime *is* the worker's: no name to
allocate, no `docker rm` to forget, and no way to leave one running after the
session that owned it died. One per session, not one per command, because a
container per command is a start — on some runtimes a VM boot — per command.

**What crosses the pipe is the policy, not the sandbox.** A resolved `Sandbox` is
canonical paths on a filesystem the worker does not have; a `SandboxPolicy` is
portable, and the far side runs the same `Sandbox::new` the host would.
`Sandbox::to_policy` is the inverse, and a test asserts the round trip grants
what the original granted. The task's `Authority` crosses too, so a denial from
inside still says *the approved plan for task 7* rather than *the sandbox
policy*.

**The base is mounted at its own absolute path**, not `/workspace`. Paths appear
in verdicts, prompts, tool results and the record; translating them would make a
contained run and a host run of the same script differ in their bytes, and "one
flag apart" is what every probe in `scripts/tasks/` depends on. `[[worker.paths]]`
is for the other direction — trees that exist only inside the image, added to the
policy the worker resolves and never resolved on the host, because a granted path
that is not there is a load error and `/usr/local/cargo` is not a directory on a
Mac.

```toml
[worker]
runtime = "docker"          # host | direct | docker | podman | nerdctl | container
image = "luu-worker:dev"
timeout-ms = 30000          # the seam's patience for a call with no clock of its own

[[worker.paths]]            # the image's toolchain, resolved only on its side
path = "/usr/local/cargo"
access = "execute"
```

- **A runtime is a name, not an integration.** This layer builds an argv, which
  is the same shape `commands` already has, so Docker, Podman, `nerdctl` and
  Apple's `container` substitute for each other and the dependency becomes a
  choice. Where the flags are *not* uniform it says so rather than assuming:
  `--user uid:gid` against `--uid`/`--gid`, and Apple's runtime having no
  `--network none` at all — under which the container stays attached, the
  per-command seccomp filter is doing all the denying, and the verdict reports
  it.
- **`runtime = "host"` is the default** and is every run this repository has
  measured. `direct` runs the worker as a plain child with no container: it
  isolates nothing, says so in every line that reports it, and exists so the seam
  is testable where no runtime is installed and so a failure can be attributed to
  the IPC or to the container in one flag.
- **The image is declared, not generated** — `Containerfile` in the tree, and
  `luu.container.toml` is the wide-open posture, a separate file so that in three
  weeks nobody has to tell a default from a leftover. `scratch`/distroless is
  wrong here: the container's whole job is running `cargo`, `rustc`, `git`, `rg`
  and `ls`, and a minimal image has none of them.
- **`commands = [...]` is the image's manifest.** The worker is the only process
  standing on the image's `PATH`, so its handshake answers which allowed commands
  the image actually has, and `luu tools` prints the gap under `absent` —
  "granted by the policy, absent from the image", a third failure mode distinct
  from *denied by the policy* and *the kernel will not hold it*. The handshake
  also carries the protocol number and the worker's own `luu` version, because an
  image is the easiest thing in this design to leave stale.
- **The container isolates tool execution only**; the context manager and the
  model client stay on the host. The GPU is the obvious reason and not the strong
  one: the topology does not justify `network = false`, it is what makes it
  *affordable*. A model call born inside the container could not be made with the
  container's network off, and the build-script protection would be lost at the
  moment isolation was raised. Under local-first this weighs more, not less.
- **The network is decided once, at creation, and never changed.** `--network
  none` when the session's policy denies it, attached when it allows it. Nothing
  toggles a running container's network: the filter is built at every spawn, so a
  *task's* grant is scoped to exactly the task that declared it, with no startup
  cost and no window that outlives it. Connecting and disconnecting by hand would
  move what the kernel already decides per call.
- **Egress through a host-side proxy (`EgressProxy`)** filters outbound destinations when
  `network: true`. A plan carries `egress: ["crates.io", "*.github.com"]` (or script directive
  `## egress: ...`), and an in-process HTTP CONNECT proxy inspects target hostnames, tunneling
  matching traffic via `copy_bidirectional` and returning 403 Forbidden for unapproved destinations.
  Standard proxy environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and lowercase)
  are injected into `run_command` subprocesses. See
  [`RECORD/2026-09-04.egress-through-the-host.completed.md`](RECORD/2026-09-04.egress-through-the-host.completed.md).
- **The loop holds one clock over every tool call, because a clock inside the
  thing being timed cannot time it.** `run_command`'s own timeout covers its child and nothing else — not
  a wedged tool, not the worker's loop, not the runtime between them — so a
  worker that was *alive and stuck* used to hang the turn, the job and the
  session with nothing anywhere saying why. The host's deadline for a call is
  **the call's own clock plus `[worker] timeout-ms`** (default 30 000, the same
  number `run_command` defaults to), where every tool without a timeout of its
  own contributes zero: so the seam can never fire before the tool's own clock
  has had its chance, which is what makes it safe to have on by default. When it
  fires the loop drops the call and tells the executor to deal with what is left
  (`Executor::abandon`): the worker is **killed** rather than merely abandoned —
  a late answer on a reused pipe would pair the previous call's outcome with the
  next call's request, two well-formed lines and nothing to say they were swapped
  — and the next call starts a new one, which is sound only because a worker
  holds no state between calls. The same replacement now covers a worker that
  exited, which previously took the rest of the session's tools with it, and
  `luu tools` prints how many workers a session has had to replace. **The clock
  is the loop's and not the seam's** because `runtime = "host"` is the default
  and has no seam: the loop is the only thing above both places a tool can run.
  For that deadline to be real the in-process tools had to stop blocking inside
  their own future — `std::fs` inline in an `async` block never yields, so the
  timer beside it is never polled and the timeout never fires — so every
  filesystem syscall now runs on a blocking thread it can be dropped from. The
  thread itself is not cancellable, which is why `bin/luu.rs` bounds runtime
  shutdown: otherwise a wedged read the turn survived comes back as a process
  that will not exit. See
  [`RECORD/2026-09-05.a-clock-where-there-is-no-seam.completed.md`](RECORD/2026-09-05.a-clock-where-there-is-no-seam.completed.md). And `timeout_ms` is
  itself capped at ten minutes, because it is the *model's* number: everything
  else in a call is checked against the sandbox, and this one was checked against
  nothing. See
  [`RECORD/2026-09-05.a-clock-at-the-seam.completed.md`](RECORD/2026-09-05.a-clock-at-the-seam.completed.md).
- **It runs on Linux, in CI, on every push.** `scripts/container-check.sh` builds
  the image and walks it — the handshake, the policy's commands against the
  image's manifest, a read inside the tree, the same read on the host with both
  recordings compared field by field, a denial, and a `run_command` — and the
  `container` job runs it on `ubuntu-latest`. Landlock **ABI v7** + seccomp +
  rlimits hold the child inside stock Docker, with no `--privileged` and the
  default seccomp profile. Until 2026-09-08 every contained run in this
  repository was Docker Desktop on macOS, by hand, once. See
  [`RECORD/2026-09-08.the-container-on-a-runner.completed.md`](RECORD/2026-09-08.the-container-on-a-runner.completed.md).
- **A session picks its posture, and the container's lifetime is the session's.**
  What is chosen is not a runtime but a *posture* — the policy file, which
  decides the sandbox and the seam together, on purpose — named in the state
  directory's `config.toml` beside the providers:

  ```toml
  [posture.container]
  policy = "luu.container.toml"
  ```

  `POST /api/sessions {provider, model, posture}` resolves it **before** anything
  is reset, so a runtime that is not installed refuses the new session rather
  than ending the one that is running; the worker the previous posture started is
  then ended rather than dropped, because `kill_on_drop` gets to a container
  "eventually". The browser never types a path, for the reason it never types a
  URL: what may be chosen is bounded by what somebody wrote on this machine, and
  the names are read once when the server starts. **A resume may not move it** —
  a destination is where a session sends and a posture is what it may do, and its
  jobs were approved against this one. `luu chat` and `luu stdio` are unaffected:
  there the process is the run, and its policy file is a flag. See
  [`RECORD/2026-09-08.a-session-picks-its-executor.completed.md`](RECORD/2026-09-08.a-session-picks-its-executor.completed.md).
  Starting the worker with the session was chosen before anyone had a number.
  The number is ~100 ms over a host session, on machine 1 with the image already
  built — warm, and small enough that nobody would reverse the choice on cost.
  See [`RECORD/2026-09-15.what-a-container-start-costs.completed.md`](RECORD/2026-09-15.what-a-container-start-costs.completed.md).

  **What that rule compares, since 2026-09-17.** The fold keeps the posture its
  stream's last header names, and a resume refuses — 409, naming both sides —
  when the server is not in the same place: the **three facts**, never the name,
  because the file behind a name can be edited tomorrow. A body may name a
  posture and is admitted only when it resolves to those same three facts, so
  the endpoint says *which one you mean* rather than *move this session*; the
  page reads it off the session and says it back. It refuses rather than
  rebuilding the posture itself, because building one can start a container and
  a click on a row in the history is not a request to start one. A stored
  posture of `None` is **unknown** — a recording from before format 10 — and is
  let through rather than guessed at; the retarget header, which is now written
  when the posture moves and not only when the destination does, is then the one
  thing that says where the rest of those turns ran. Before this the endpoint
  refused a *named* posture and inherited a different one silently. See
  [`RECORD/2026-09-17.what-an-approval-was-granted-under.completed.md`](RECORD/2026-09-17.what-an-approval-was-granted-under.completed.md).
- Still ahead: `--cap-drop=ALL`, a pids cgroup in place of `RLIMIT_NPROC`, and
  concurrent sessions — one live session per `serve` is still the shape, so one
  container at a time is still the shape.

## VSCode integration

- Use the **VSCode Chat API** (`vscode.chat.createChatParticipant`) — requires a lightweight TypeScript extension.
- The TS extension acts as a bridge: renders the UI (messages, plan approval) and talks to a **spawned `luu stdio` subprocess** — one process per window, no port, no bind, no token, speaking line-oriented NDJSON with errors on stderr. Not a socket: the moment there is one, `approve_task` is reachable on it, and signed approvals become a precondition rather than a later step. The socket transport stays what `serve` already offers and becomes interesting for the editor only once multi-session exists. See [`RECORD/2026-09-01.how-a-surface-reaches-the-engine.completed.md`](RECORD/2026-09-01.how-a-surface-reaches-the-engine.completed.md), [`RECORD/2026-09-04.protocol-over-stdio.completed.md`](RECORD/2026-09-04.protocol-over-stdio.completed.md), and [`RECORD/2026-09-04.vscode-extension.completed.md`](RECORD/2026-09-04.vscode-extension.completed.md). Surface #3 lives in `editors/vscode/`.
- The task's confirmation step maps well to this UX: an editable plan before execution (similar to how Copilot shows changes before applying them), then execution with streaming of which tool is being used, and a closed task collapsing to its summary in the thread.

## Debug web client (agent protocol)

A local web UI (chat, session browser, context inspector) is the fastest way to see what the
context manager is actually doing — the CLI can't show a token budget or a prompt diff.

**Three columns, and each has a head, a body and a foot.** Inspector, content, chat — above
1260px; below it, the inspector and *one* of the other two, with a toggle in that column's own
head. Which of the two is a person's choice rather than the viewport's, and a click that opens
something in the content column switches to it, because the click already said which column
they want. `General → layout` pins the two-column mode at any width. The heads and feet are all
40px, one token (`--chrome-h`), with one exception: the chat's foot is two rows, because what
you are sending and what you are sending it *under* — manual or automatic confirmation, which
provider and model — are two different questions, and the second was not on the page at all at
the moment somebody typed a prompt.

There is **no page-wide header**. There was, and every control on it belonged to a column: the
two destination tags and the status word are the chat's session, the fixtures picker is its
replay, the hamburger opened preferences about the browser. A strip of other columns' controls
cost every column 3rem of height to say so, and it was the one thing on the page with a fixed
height, so the grid was `calc(100vh - 3rem)` — a magic number that was wrong the moment it
wrapped. What each 40px carries now: the inspector's head is the `luu` logo (which is a control
— it means *back to the conversation*) and the panel tabs, its foot the chosen folder, the
status word and **settings**; the content column's head is one tab per open thing and its foot
that thing's own facts — and, the way VS Code does it, a single click opens a **preview** tab, in
italics, that the next single click replaces in place, while a double click (on the row, or on
the tab) or the tree's pencil keeps it; the chat's head is the session's name, editable in place, beside
new-session and history. Settings sits in the inspector rather than in the chat's head because
the chat's head is the one head that disappears: below 1260px the second column is a choice, and
with the content showing there was no settings button on the page at all.

The inspector switches between the workspace's file tree, its git changes, and the
context-manager panel that used to be the whole right column. The content column holds **one tab
per open thing** — a file, a diff, or a block of debug text such as a prompt at the width it was
written for — so looking at a diff no longer loses the file on screen. A tab holds its identity
and not its bytes: activating one re-fetches, because the server answers the largest file in
this repository in 20 ms and what a file costs is building the rows — which a cache would not
save. Those rows are **built in plain JS by `rows.js`, not by a template**: a `:each` inside a
`:each` charges a scope and two effects per coloured span, over spans that arrive whole,
already coloured, and never change. Dropping the renderer from that one subtree halves the time
to the whole file and takes the tab's JS heap from hundreds of megabytes to about twelve. See
[`RECORD/2026-09-16.the-viewer-in-plain-js.completed.md`](RECORD/2026-09-16.the-viewer-in-plain-js.completed.md). Read-only throughout: the file tree and the git panel are a window onto the workspace,
not a second way to change it — every write still goes through the job gate. A tree row is an
`<li>` holding three controls rather than one big `<button>` — the button that opens the thing,
git's letter, and an **edit pencil that appears only under the pointer** (and under keyboard
focus, because an action that exists only on hover is one a keyboard cannot find). Its slot is
reserved rather than conjured, so revealing it does not reflow the name being pointed at. **The
pencil opens the file today**: the write half is not decided, and deciding it means arguing with
the sentence above rather than routing around it. See
[`RECORD/2026-09-16.the-tree-makes-room-for-an-icon.completed.md`](RECORD/2026-09-16.the-tree-makes-room-for-an-icon.completed.md).

The columns' contents and every dialog are separate jq79 components
(`inspector-{files,git,debug}.html`, `content-viewer.html`, `settings-{modal,general,models}.html`,
`session-starter.html`, `folder-picker.html`); `app.html` keeps the shell — the grid, the three
columns' chrome, and the chat. Two things inside `content-viewer.html` are attached to plain
elements by id rather than drawn by the renderer, for the same reason and now with a number
behind it: the optional Monaco editor, and the own viewer's rows.

**Every component's styles are `<style scoped>`, and what more than one needs is in `app.css`.**
jq79 rewrites a scoped selector to also match `[data-jq79="…"]`, which only the elements that
file's own template built carry, so `.name` in the chat and `.name` in the git panel are two
different rules and both are right. `app.css` holds what that isolation would duplicate: the
palette and the tokens, the controls (`input`, `button`, `button.link`, `pre`), `.ic` and the
`.hl-*` captures, the modal chrome, and the three words that carry a volume — `.dim`, `.warn`,
`.error`. Each rule there carries a comment saying why it is not in a component, because *global*
is the exception on this page and has to argue for itself. Two consequences are load-bearing and
easy to rediscover the hard way: a scoped rule is one specificity level above the plain class it
was written as, so a shared rule that has to win anyway is written to outrank it
(`button[disabled][disabled]`, not `button:disabled`); and a class that *cannot* be scoped — the
viewer's `.code-rows`, built by `rows.js` rather than by the compiler — is named as though the
whole page could reach for it, because it can. See
[`RECORD/2026-09-16.styles-that-stay-in-their-component.completed.md`](RECORD/2026-09-16.styles-that-stay-in-their-component.completed.md).

Preferences live in `prefs.js` and in `localStorage`: theme
(auto/light/dark, where auto is `prefers-color-scheme` resolved live in JS rather than a second
copy of the light palette), which editor draws a file, the layout, and who answers the gate.
None of it is in `config.toml`, which says where a run *sends* — a fact about the run, not about
the screen somebody reads from. Dark stays the default for a browser with nothing remembered,
because every screenshot and every record here was taken against it.

**The chat has a name, not a tab strip.** One session is on screen, so a strip was spending a row
to answer a question the head answers in words; the strip lives on as a history popover, and the
name is editable in place — `PATCH /api/sessions/{id}`, the only thing the page may change about
a stored session other than deleting it. It refuses an empty title, and it patches the two places
a title lives (the listing's column and the fold's blob) rather than going through `save`, which
replaces a whole row and would drop the `provider` the session picker starts from. Deleting is
still the × that arms on the first click and deletes on the second, and it is not drawn on the
live session or the active one, which the server refuses to delete either way.

**Which folder, chosen under the one `serve` was started in.** On a first visit the page asks,
over the whole window and with no way out but choosing — there is nothing behind it yet — and
remembers the answer in `localStorage`, keyed by the base. What is chosen is a *subdirectory* of
where `luu serve` started: nothing widens, because `/api/workspace/*` already resolves every path
through `Sandbox::check_path` against that base and the root is a prefix the page narrows what it
*lists* by. A picker that could climb above the base would be a browser control that moves the
sandbox. Paths stay base-relative everywhere they are sent, and the root is applied to what is
shown — a second spelling for the same file is a second chance to get the prefix wrong.

**What is not there is said where it is missing.** No providers at all: the chat column's body is
a placeholder with the one button that fixes it, opening Settings on Models — rather than a modal
that opens itself over the folder question. A model the provider answered a list without: the
composer is disabled and the foot says which, because that is certain and costs a whole turn at
the far end to discover. A provider that did not answer at all: the foot says so and the composer
stays enabled, because a provider that is starting up is an ordinary state and locking the
composer over one would be wrong more often than right.

**Every modal is a `<dialog>` opened with `showModal()`** — settings, the folder picker, the
session starter — so ESC, the focus trap, everything behind it inert and the `::backdrop` are
the platform's rather than three hand-built copies of a `z-index: 50` div. What the platform does
not own is *whether* a modal is on screen, which is the page's state, so `modal.js` routes every
native dismissal back to the same close function the close button calls; the folder picker on a
first visit refuses both, because there is nothing behind it to go back to. **ESC walks a ladder**
— a modal cancels itself and the page does nothing else on that keypress, then the history
popover, a rename in progress, an armed delete, and finally, with nothing left to cancel and two
columns on screen, it swaps content and chat. A running turn is deliberately not on it: it is the
one cancellable thing whose undo costs work. `asModal` is called from a component's `:setup`,
which runs to completion *before* the template renders, so it **waits across frames** for the
element rather than looking once: looking once made opening the settings modal a race against its
own setup — one `await import` per section, and jq79 renders only after all of them resolve. Two
sections won that race and a third lost it, and losing it was silent, because a `<dialog>` that
was never shown is in the document and invisible. See
[`RECORD/2026-09-16.the-modals-are-dialogs.completed.md`](RECORD/2026-09-16.the-modals-are-dialogs.completed.md).

**Settings is sections down the side**, not one scroll, because the sections are not steps:
*General* (theme, editor, layout, which icon theme drew the tree, which folder), *Models*
(what this server resolved, then the providers file the next run reads — with the built-in mock
as a row of its own, chosen when the file names no default, because a server that fell back to
it used to show a table with nothing chosen; a new provider is a form in a modal that writes that
one profile and nothing else), *Engines* (the model servers luu starts — see [A model server luu
starts](#a-model-server-luu-starts)) and *Resend* (the three
rules that decide how much of the history a turn pays for again). *Resend* is beside *Models* and
not inside *General* on the same rule that keeps `config.toml` and `localStorage` apart: General
holds facts about the **screen** somebody is reading from, and one server answers two people on
two machines, while these three change the bytes the model receives. A preference that changes
the prompt is not a preference. It shows what the live session is rendering under, the machine's
`[resend]` table below it, and says where the two disagree — a session started under a rule
nobody wrote down is the ordinary case. It also carries what each rule *bought*, including that
two of the three have no accuracy measurement at all, because this panel is where somebody
decides whether to turn one on — and rule C's row says which half of its number is measured,
since `cited_reads` is the whole of it and `cited` is that plus an unchecked claim. A save that the running session was only partly moved onto says
which rules it kept and why, in the server's own words rather than a rule re-derived in the
browser. The *Files* row is there
for a reason worth keeping: nothing on the page named `[ui] icon-theme` at all, and the first
person to meet an unconfigured tree read a working fallback as a broken panel. The `editor` setting offers Monaco where somebody installed
it — an **npm dependency of `web/`**, gitignored, excluded from the `rust_embed` folder
and served from disk by one route, so a release binary does not carry several megabytes of an
editor that is off by default. That does not reopen
[`RECORD/2026-09-16.what-the-debug-ui-does-not-need.completed.md`](RECORD/2026-09-16.what-the-debug-ui-does-not-need.completed.md),
which measured Monaco and kept this page's viewer as the default; it adds the things this viewer
cannot do — folding, jumping to a symbol — as an opt-in that costs nothing when it is off. A
checkout that ran `cargo build` and nothing else says so and stays on the viewer, the same shape
`[ui] icon-theme` has.

**The chrome is drawn, not typed.** `app.html` also carries one `<svg>` sprite — a caret, a
refresh, a menu, a close, a plus, the file/folder pair, and the chat head's and settings rail's
own seven (clock, cog, sliders, server, repeat, clip, speech bubble) — and every symbol on the page is a
`<use>` of it, stroked in `currentColor` so it takes the colour of the control it sits in and
needs no second set for the light theme. These were text glyphs (`↻`, `▾`/`▸`, `▪`/`▫`, `≡`),
and a text glyph's size, weight and baseline belong to whichever font on the machine carries
that codepoint, so none of them lined up with the 16px box a theme's file icons occupy. The
exception is prose: the `→` in `use →` is a word, not a control, and stays a character.

The panels are fed by four plain `GET`s — `/api/workspace/{tree,file,git-status,git-diff}` —
which are **not** behind the job gate on purpose: a person clicking a directory is not a model
proposing a tool call, and routing it through approval would mean either rubber-stamping a job
nobody asked for or building a second, unaudited read path. Every path still resolves through
`Sandbox::check_path`, so the panels reach exactly what the session's policy grants and nothing
above it. Ignored entries come from the difference between what `ignore`'s walker yields and
what `read_dir` does, which gets nested `.gitignore` files and negations right for free. The
diff is parsed into hunks server-side rather than shipped as text — the same call this document
makes for prompt diffs below, answered by parsing git's own output rather than adding a second
diff implementation.

**The viewer opens in two blocks.** A file arrives whole and already coloured, and what costs
is the DOM: about 50 µs per row, measured, **with or without colour** — so a 3807-line file is
~20 ms of server and ~300 ms of browser. The page renders a screenful first and everything
after it one frame later, as **two `:each` groups over two arrays** rather than one array that
grows: jq79 rebuilds an `:each` when its array changes, so a growing array would rebuild the
rows somebody is already reading, losing their selection and costing the work again. Two
groups in one `<ol>`, so the CSS counter numbers the second block without being told where it
starts, and a spacer the height of the rows still to come, so the scrollbar is the length of
the file from the first frame and never jumps. Something readable arrives in a third of the
time; finishing costs 8–28% more, which is the right way round for a panel somebody is
looking at. This is also the answer to "colour it in the browser with WebAssembly,
progressively": the colouring is the cheap half, and moving it into the page would add work to
the thread that is already the bottleneck. The grammars' queries are compiled once at startup
rather than per request, which is what took a small Rust file from 15 ms to 0.6.

**Icons and highlighting follow the same two rules.** File icons come from a VSCode icon theme
this machine already has, named by `[ui] icon-theme` in `config.toml` — an extension directory
or a theme JSON — and nothing is vendored: the page draws its own file and folder until
somebody names one, the same way the sandbox waits to be told where `~/.cargo` is. The theme is
read into an id → path table and `/api/icons/{id}` serves only ids in it, so no client-supplied path ever
reaches the filesystem. **Settings → General → Files chooses it** from the themes this machine has
(imports, and VS Code, VSCodium, Cursor and Windsurf extensions), from a typed path, or from a folder
picked in the browser and imported into `<state dir>/icon-themes/`. A choice is written to
`config.toml` only if it loads, and is swapped in live. The manifest's `revision`, in every icon
URL, keeps the browser's cache honest. A versioned extension path that an update removed resolves
to the newest version beside it. See
[`RECORD/2026-09-28.an-icon-theme-from-settings.completed.md`](RECORD/2026-09-28.an-icon-theme-from-settings.completed.md). Syntax highlighting is `tree-sitter-highlight` on the server, sixteen
grammar crates — `Makefile`, `Containerfile` and `Dockerfile` found by name, having no extension
to be found by — sent as **pre-sliced chunks rather than offsets** — a byte offset from Rust read as a
UTF-16 index in JavaScript agrees until the first non-ASCII character and then silently does
not. Injections are on, so a fenced block in Markdown is highlighted as its language and HTML's
`<script>` and `<style>` as JavaScript and CSS. TypeScript's query is its own followed by
JavaScript's, which it inherits. **Where two patterns capture the same node, the later one
wins** in this version of `tree_sitter_highlight` — measured on make's query, whose Neovim-era
names (`@conditional`, `@include`) this page does not colour, so the patterns that rename them go
*after* it. Markdown is highlighted in **two passes**: the block grammar,
then the inline grammar over each `inline` node, laid on top. The inline grammar highlights
nothing when `tree_sitter_highlight` runs it as an injection. The capture names map to this
page's own palette, so by default the viewer follows the light/dark toggle. The one exception is
opt-in: the `code` preference can choose **Monokai** (Sublime Text 3's), which brings its own
dark ground and covers Monaco too. The payload reads head-first — `path`,
`language`, `truncated`, `total_lines`, then `lines` — because `serde` writes fields in
declaration order and the facts about a file have no business sitting behind 681 KB of it.
`total_lines` is counted before the 512 KB cut, so a truncated file says *6255 of 20001 lines*
rather than only that it was cut. See
[`RECORD/2026-09-15.a-three-pane-inspector.completed.md`](RECORD/2026-09-15.a-three-pane-inspector.completed.md)
and
[`RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`](RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md).

**v1 of the message enums is frozen.** `ClientMessage` is `prompt`, `cancel`,
`approve_task`, `reject_task`, `close_task`, `reopen_task`; `ServerMessage` is
the eleven the server emits, the task lifecycle included. It was frozen only once
every one of them had been watched being sent and answered rather than imagined
— the record format moved to 3 with it, and a change an older reader could not
make sense of bumps both from here.

### Transport: HTTP + WebSocket, not REST-only and not gRPC

**Decision: one JSON message schema, several transports (stdio, WebSocket), served over plain HTTP.**

- **Not gRPC.** Browsers can't speak gRPC natively — it needs grpc-web plus a proxy (Envoy) or
  Connect. `tonic` also forces a protobuf IDL maintained in parallel with the `serde` types, with
  codegen in `build.rs`, and binary frames that are unreadable in devtools. gRPC pays off for
  cross-language, multi-service, high-throughput systems; this is one local process talking to one
  browser. The cost is all setup and the benefit is nil.
- **Not REST-only.** No streaming of tokens or tool events; polling makes a debug UI useless. Adding
  SSE on top gets you halfway to a WebSocket with more moving parts.
- **Reuse what stdio already needs.** The VSCode bridge already requires a JSON-lines protocol.
  Define the messages once as `serde` enums (`#[serde(tag = "type")]`), put the transport behind a
  trait, and the web client and the extension speak the same protocol — one schema to version, two
  clients debugged at once.

### Server shape

`luu serve --bind 127.0.0.1:7878` — loopback by default and unauthenticated; **any other address
requires a bearer token, and without one the bind is refused rather than warned about.** The check
runs before the listener exists, so an unauthenticated non-loopback server is not a state the
program can be in. The token comes from `--auth-token-file <PATH>`, whose mode is checked (a flag
is greppable in `ps`, an env var is inherited by every child `run_command` spawns). It gates `/ws`,
which carries task approval, and `/api/*`, which carries this session's prompts and source; it does
not gate the embedded page, which is the same bytes in every copy of a public binary and which a
browser cannot request with a header. `Authorization: Bearer <token>` everywhere, plus `?token=` on
`/ws` alone, because the browser's `WebSocket` constructor cannot set a header. See
[`RECORD/2026-09-01.what-the-audit-left.completed.md`](RECORD/2026-09-01.what-the-audit-left.completed.md).

The page lives in `web/` at the root of the repository, beside the crates rather than inside `luu`, and
is embedded in the binary with `rust-embed`, so there is one command, one URL, and no node
process in the loop.

Live channel — `WS /ws`:

| Direction | Messages |
| --- | --- |
| client → server | `hello`, `prompt`, `approve_plan`, `decline_plan`, `request_plan`, `close_job`, `reopen_job`, `cancel` (with `approve_job`/`reject_job`/`*_task` aliases) |
| server → client | `hello`, `turn_started`, `token`, `tool_call`, `tool_result`, `ended`, `failed`, `draft_opened`, `plan_proposed`, `plan_declined`, `job_approved`, `job_closed`, `job_reopened`, `refused`, `evicted`, `grounded` — all built, protocol v7 (record format 19); `job_proposed` and `job_rejected` still parse and are never written, so an older recording replays; `context_snapshot` is still ahead |

**Both numbers live in two languages, and a test reads both.** `store.js`
declares `PROTOCOL` and `FORMAT` beside `agent_core::protocol::VERSION` and
`agent_core::record::FORMAT`; `crates/luu/tests/ui_versions.rs` asserts each
pair, because when `FORMAT` went to 8 and the page still said 7 the host refused
every `hello` the page sent and the UI could not open a session at all. A
version refusal now also stops the page's reconnect loop, which had been turning
it into the word "closed" and nothing else. See
[`RECORD/2026-09-08.a-test-that-clicks-approve.completed.md`](RECORD/2026-09-08.a-test-that-clicks-approve.completed.md).

**The wire says what it speaks, in both directions.** The server's `hello` has always
carried `protocol`; the client's now answers with its own, and with the record format it
can read. A mismatch in *either* direction is a `refused` with reason `version` and then a
close — a newer client is the case the host cannot parse and an older one is the case the
client cannot, and neither side guesses. It is **required as the first message on a port
that asks for a bearer token** (which is every port off loopback, since binding one
without a token is refused) and optional on loopback and over stdio, where the peer is
this machine's own browser or the process that spawned us. The server's `hello` also
carries the `session` id, because an approval is signed against it.

**An approval can be signed.** `approve_plan` carries an optional
`signature: { by, sig }` — Ed25519 over a canonical rendering of *the grant* (session, job,
`files`, `writes`, `commands`, `closes_on`, `network`, `egress`), so a relay that widens
the grant between the person and the gate invalidates what it is relaying. The
`job` in that grant is **the id the approval is about to hand out**, not one that
exists — it is the one thing the gate still names, and only because a signature
bound to no job could be replayed against any of them. The server refuses a
signature that names an id other than the one it is about to open, and an
unsigned approval names nothing at all. The verifying
keys are named in `luu.toml`:

```toml
[approvals]
required = true          # an *unsigned* approval is refused; a *wrong* one always is

[[approvals.key]]
name = "jgermade"
public = "ed25519:…"     # luu key new --out ~/.luu/approval.key
```

`luu key new` makes a key (private half written `0600`) and `luu key sign` signs an
`approve_plan` read on stdin, so the signing half lives where a remote operator's `luu` can
call it rather than inside one surface. `job_approved` then carries `approved_by`:
`operator` for the local unsigned case, `key { name }` for a verified signature — beside
`closed_by` and for the same reason. Three failures are one `refused` with reason
`signature` and a `detail` that says which: unsigned where a signature is required, a name
no key answers to, and bytes that do not verify. A bearer token answers *who may reach the
port*; this is the first thing that answers *who approved*. See
[`RECORD/2026-09-04.signed-approvals.completed.md`](RECORD/2026-09-04.signed-approvals.completed.md).

Closing a job is an event, not a mutation: reopening one is folding the log
differently, never undoing a deletion. Freezing v1 of these enums waits on the job
lifecycle for that reason — it is the last cheap moment to add it.

Read side — plain GETs, browsable and curl-able. Every path answers for the live
session, under `live` or under its own stored id, **and for any session the store
has** — so a second `serve` pointed at the same database can be asked what the
first one did:

- `GET /api/sessions` · `GET /api/sessions/:id` · `POST /api/sessions` · `DELETE /api/sessions/:id`
- `GET /api/sessions/:id/turns?from=&limit=`
- `GET /api/sessions/:id/turns/:n/prompt` — the exact string sent to the model
- `GET /api/sessions/:id/context` — current token budget breakdown
- `GET /events?session=` — SSE mirror of the WS stream, so a session can be followed with `curl -N`
  without a browser

Server stack: `axum` + `tokio`.

### UI stack

**[jq79](https://github.com/jgermade/jq79) — single-file, no compiler, zero dependencies.**

The constraint that decides this is the build pipeline, not the framework's ergonomics. A bundled
frontend (Vite + React or otherwise) forces one of two bad options: `npm` inside `build.rs`, so
`cargo build` requires node, or a committed `dist/` with the noise that brings. jq79 has no compiler
step — components are `.html` files loaded at runtime — so `rust-embed` ships one `jq79.js` plus a
handful of `.html` files and the build stays pure Cargo.

The rest follows from that:

- **Dev loop**: `jq79 dev` hot-reloads components with no bundler. Serve the directory from disk in
  debug builds (`#[cfg(debug_assertions)]`) and from `rust-embed` in release — the dev server applies
  no transforms, so both paths serve the same bytes. No Vite proxy to configure.
- **Rendering**: proxy-based fine-grained reactivity with no virtual DOM, and `:each` with `:key`
  keeps existing DOM when the transcript is appended to.
- **Tool calls in the transcript**: an assistant reply is drawn as prose and tool calls, not as
  the ```` ```tool ```` blocks the model typed. `segments.js` splits the turn's text at render
  time, using the same forms `parse_call` accepts (a fence, or a bare `{"name": …}` at the start of
  a line). It pairs each block, in order, with the turn's `tool_call`/`tool_result`, and
  `tool-call.html` draws each one as a card: the name, what the call is about, how it went, and
  the arguments and output behind a click. A block still being written is a placeholder, and a
  tail that could be the start of one is held back for the frame or two it takes to tell. Nothing
  on the wire or in the store changed. The text is the concatenated tokens live and after a
  reload alike, and the raw text is still what the inspector shows. See
  [`RECORD/2026-09-29.a-switch-that-does-not-wait.completed.md`](RECORD/2026-09-29.a-switch-that-does-not-wait.completed.md).
- **A turn's meta line is its flow and the way into it.** Under each answer, one line:
  `speed · → sent ← received · 🕑 first token → last token`, with how the turn ended only
  when it did not simply stop. It is at 0.6 opacity until hovered, when it is whole and its
  symbols take their colours. The speed is
  completion tokens over first-token-to-end (`timing.js`): the wait before the first token is its
  own number because it holds a model loading, and a model switched to should not look slow for
  it. A turn that ran tools spent part of that span running them, so its speed is a floor and
  reads `≥`. No count from the backend means no speed, never zero. The times are the page's clock
  live and `TurnView`'s `at_ms` after a reload. `first_token_at_ms` is folded from the `token`
  lines' own stamps, so every recording has it and no format moved. A click opens
  `turn-inspect.html`, a `<dialog>` with three tabs: the context, the answer as the model wrote
  it (fences included), and each tool call. **The context is grouped by what each piece is**
  (`context-view.html`): system, tools, map, summaries, history, code, prompt, in the legend's
  order and colours, each a card under its bucket's count, with an earlier exchange keeping
  who said it. The boundaries are read back from what the renderer always writes (`# Tools`,
  `# Repository map`, a fold's `[job closed]`, a `// path` fragment, the question at the end),
  in `prompt-parts.js`, because the wire carries counts and not spans. **No block scrolls unless
  asked to**: one too large to read at a glance is clipped with a fade and offers *show all*
  (inline) or *scroll* (a box of its own). Nothing is fetched: it is the turn `state.history`
  already holds.
- **The composer: attach on the left, the context on the right.** The clip that names the open
  file in the prompt sits left of the input. The right end of the options row is the context
  indicator: the newest turn's buckets as a small bar in the legend's colours and the tokens sent
  over the window, `1.5k / 8k`, the count amber from 80% of it and red from 95%. Where the window
  is unknown it reads `1.5k / —` and says why. The turn modal and the context modal carry the
  same partial over total. The indicator reads the newest turn whether it happened live or came
  back from the server, so a reload does not leave it empty. A click opens `context-modal.html`, the same grouped view
  about that turn, with the window's use, the answer's reserve and what the server counted. See
  [`RECORD/2026-09-30.the-foot-and-the-turn.completed.md`](RECORD/2026-09-30.the-foot-and-the-turn.completed.md).
  **What is sent is on screen at once**, dimmed, until the server's first answer to it
  (`turn_started`, a refusal, a plan at the gate) draws the real thing. It does not wait for
  `turn_started`, because a slow server made that look like a lost message. See
  [`RECORD/2026-10-01.a-slow-disk-is-one-slow-request.completed.md`](RECORD/2026-10-01.a-slow-disk-is-one-slow-request.completed.md).
- **Components**: `web/components/` holds components that import nothing from the page.
  Everything arrives as props or slots, everything leaves as `:model` or `$emit`, styles are
  scoped, and page tokens are read with fallbacks. Three so far, each taking an idea from rare-ui
  with none of its code: `Segmented` (a pill that slides to the chosen answer), `Rail` (a bar beside the chosen section that springs
  to it; it was a dot until 2026-09-30) and `SaveButton` (a count of unsaved changes, and a tick once a
  save lands). Every segmented choice on the page is a `Segmented`, and the global `.choice` rule is
  gone. See
  [`RECORD/2026-09-28.components-of-our-own.completed.md`](RECORD/2026-09-28.components-of-our-own.completed.md).
- **Virtualization**: no library, and none needed — bind `:each` to a computed window plus two spacer
  elements. `GET /api/sessions/:id/turns?from=&limit=` already puts pagination on the server.

Two consequences worth designing around rather than discovering:

- **Compute the prompt diff in Rust** (`similar`), and send resolved spans over the protocol. Pulling
  in CodeMirror or Monaco for it would reintroduce the bundler this choice just removed, and the
  server holds both prompt strings anyway — it is the right place for the work. Should the editable
  plan later need a real editor, `await $mounted()` + `$self(...)` mounts an imperative widget
  cleanly, so the option stays open without being paid for now.
- **No compile-time check of the protocol types**, which a TypeScript build would have given. Recover
  most of it by splitting the UI: the transport and store layer as a `// @ts-check`ed `.js` module
  validated against the `ts-rs`-generated `.d.ts`, templates untyped. `tsc --noEmit` stays an
  optional check task, never a build step.

### What this needs from jq79

Two gaps, neither expressible from userland, both general enough to belong upstream rather than in a
patched copy. Listed in the order luu hits them:

1. **A teardown hook** (`$onDestroy`, or a cleanup function returned from `$effect`). Effects, stores
   and injected styles are disposed with the component, but a resource the component itself owns has
   nowhere to be released. The first component written here — the one holding the WebSocket — needs
   it, as does any `requestAnimationFrame` loop or mounted imperative widget.
2. **Effect batching** (`batch(fn)`, microtask-coalesced flush). There is no scheduler; writes
   propagate synchronously, so a token stream costs one DOM write per token. Until it exists, buffer
   `token` messages and flush on `requestAnimationFrame` — worth doing under any framework.

**Promotion rule**, to keep a library whose value is "one file, no dependencies" from bending toward
its first consumer: a gap moves upstream only when it cannot be expressed in userland, or when the
same workaround has appeared twice. The two above pass the first test. List virtualization fails
both — it is ordinary component code, so it stays here until something else asks for it.

What flows back is a workload no tutorial or benchmark produces: long-lived sessions, sustained
high-frequency updates, thousand-row lists that grow at the end, and components owning external
resources.

### Debug panels that earn their place

Chat and session list are table stakes. The ones that justify building this at all
— and one thing true of every one that is built: **the panel is per turn, and any
turn can be selected**. `state.history` holds one entry per turn —
`{turn, job, budget, prefix, tools, extraCalls, prompt, dropped, cited, usage, reason}` —
filled from `GET /api/sessions/:id` when a session is opened or resumed and
appended to as each live turn ends, and the live turn is that same shape read out
of the fields the socket fills rather than a special case with its own bindings.
Selecting a turn does not follow the conversation: a person reading turn 3 while
turn 9 runs keeps reading turn 3. See
[`RECORD/2026-09-08.the-panel-keeps-the-turn.completed.md`](RECORD/2026-09-08.the-panel-keeps-the-turn.completed.md).

1. **Token budget per turn** — stacked bar of system/tools · code context · history · reserve, with
   the underlying text of each block on hover.
2. **Prefix reuse against the previous turn** — how much of the stable prefix survived,
   i.e. the prompt-cache / KV-reuse hit rate, as a share of the prompt. Built, and
   measured over *every* call rather than the first of each turn: a turn's tool
   round trips are in the same chain, so the panel says how many calls the turn
   made and what they sent instead of adding them to the template gap. The
   span-level diff of the two prompt strings is not built, and is a separate thing:
   the number says how much was reused, a diff would say what changed. `similar` is
   still right for the second and was the wrong tool for the first, which is a
   prefix.
3. **Tool call timeline** — arguments, sandbox verdict (allowed/denied and *which*
   rule matched), **who enforced it**, duration, and result size. Built. A call is
   listed when it is made and filled in when it returns, so one that is running or
   was denied reads as itself rather than as nothing happening. Result size
   *before/after pruning* waits on pruning existing.
4. **Compaction log** — when a rolling summary was generated, what it replaced, tokens saved.
   Built, and the third of those was the work: `job::Summary` carries a
   `replaced: Option<Replaced>` — which turns stopped being sent and what they
   were worth in the prompt they left — counted **at the close**, with the
   counter that counted the summary, because the window moves and the number is
   only true then. `None` is *not recorded* rather than zero, which is what a
   recording written before format 9 gets. See
   [`RECORD/2026-09-08.what-a-fold-writes-down.completed.md`](RECORD/2026-09-08.what-a-fold-writes-down.completed.md).

All four are readable for **any turn of the session**, not only the running one,
and the ones a recording carries replay the same way.

### One surface that counts, for the whole session rather than the turn

Every panel above is per turn, which is the rule they were built under and is
right for all four. It leaves a question none of them can answer: *how often did
this fire.* A reader with a recording can see that turn 6's prompt contradicted
itself and not whether that happened once or in eight of thirteen prompts — and
that second number is the one item 19's fix waits on.

`SessionView::counts()` is the answer, and the thing to know about it is that it
is **derived and never emitted**. Every figure is arithmetic over lines the
session already put on the wire, which is what keeps it from drifting from what
it counts — `luu export`'s own founding argument, one structure along: a counter
incremented at each site is a second implementation of the finding, printed
beside the finding, and believed over it the day the two disagree. It reaches
three surfaces, because every read-side thing here reaches the same three:
`luu count <record.jsonl>`, `GET /api/sessions/:id/counts` (and `counts.json`),
and `sessions/<id>/counts.json` in an export.

It counts the three window rules, the divergence, the tool calls by the rule
that decided them, the refusals by reason and the jobs with the paths their
plans granted. Two things about that list are decisions rather than coverage:

- **`renders`, not turns**, everywhere a rate appears. A prompt that
  contradicts itself about three paths went out once, and the corpus that first
  needed this number would have been read wrong by a tally that said *2
  divergences* — it is 8 of 13 prompts, of which 2 were the turn that caused
  one. See [`RECORD/2026-09-19.a-corpus-that-edits.completed.md`](RECORD/2026-09-19.a-corpus-that-edits.completed.md).
- **A total in tokens names every counter that produced part of it.** A session
  resumed onto another model keeps its turns and changes its counter, so a sum
  over it mixes two measurements; one entry is the ordinary case and two means
  the reader is told rather than left to assume.

**Grants are counted as path strings and not as files.** Which of `src/` and
`src/lib.rs` is a directory is a question about a disk; the sandbox answers it
with `is_dir()` when it resolves a root at approval, and a fold running
elsewhere six weeks later has no business asking. So item 12 of
`ROADMAP/2026-09-23` is taken this far and no further, and closing it needs a
field written where the answer is known.

There is **no panel** for any of it yet, deliberately: a number on a page is a
layout question and this is an arithmetic one, and the four above each earned
their place on their own. See
[`RECORD/2026-09-19.one-counting-surface.completed.md`](RECORD/2026-09-19.one-counting-surface.completed.md).

### The configuration modal

**Two sections, and they are not the same kind of thing.** The first is *what
this server resolved* — the provider and its URL, whether it left the machine,
the model, the window and which of the three said so (a flag, a profile's
`context-limit`, or nothing), the counter and its warning, the map and selection
budgets, the sandbox, the store. None of it is new: all of it was decided before
the listener existed and printed once to stderr, where nobody working in a
browser was standing. That includes **the window caveat**, which is the sentence
explaining why the count in the budget panel can disagree with the server's.

The second section edits `config.toml`. **On loopback only** — a bearer token
answers who may reach the port and never who may decide where this machine
sends, and a `default` provider outlives the session and the gate, which makes
it a larger authority than approving one job. Off loopback the section renders
disabled and says why.

**The browser never decides whether a URL is off the machine.** It writes what it
was given; the loader — the same one every run reads the file with — refuses a
default that leaves the machine without `remote = true`, and the refusal names
the profile and the host. The page then asks for that host to be *typed* and
retries with the declaration in place. So the rule has one implementation, and
the word is produced by a person answering the machine's objection rather than
by a checkbox the program pre-ticked. Keys are paths and never values, for the
reason the provider section gives.

**Nothing in the modal changes the session that is running.** What a session
sends to is fixed for its lifetime, and the file the modal writes is what the
*next* one resolves from. Which is now a session rather than a process — see
below. The mid-session switch stays rejected: every per-turn number is
backend-specific, and one label over two backends is a comparison nobody can
make. See
[`RECORD/2026-09-07.configuring-from-the-browser.completed.md`](RECORD/2026-09-07.configuring-from-the-browser.completed.md).

### The first run, and the session that picks a destination

**A server with nowhere to send says so, and the page opens there.** With no
`config.toml` and no flag, a run falls back to the mock — a destination that is
nowhere, and one that answers in sentences. `provider::DestinationFrom` is the
difference between the mock somebody *typed* (`--backend mock` is a choice) and
the mock a run *fell into*, `GET /api/settings` carries it as `unconfigured`,
and the browser opens the providers editor on it instead of a chat box that
would answer from a fixture without saying so. **The page never works this out
for itself** — a browser deciding it from `backend == "mock"` is the same class
of mistake as a second `is_this_machine` in JavaScript.

**A provider lists what it serves.** `Backend::models` reads Ollama's
`/api/tags` and an OpenAI-compatible `/models`, so a model is chosen from what
is actually pulled rather than typed from memory. A provider that is not running
is not an error here: the list comes back empty with the reason beside it, and
the field stays typable, because a model that has not been pulled yet is a
legitimate thing to write down.

**The destination belongs to the session, not to the process.** `POST
/api/sessions` takes `{ provider, model }` — a profile name out of the file,
never a URL — and the backend, the model, the budget and the counter are
swapped together before the new session's header is written, because a backend
swapped without its budget budgets the new destination against the old one's
window. The model offered defaults to the last one used with that profile, read
from the store's `provider` column, then the profile's own, then the first the
provider listed. **Off loopback the choice is bounded to profiles that stay on
this machine**: the write route is loopback-only because a `default` outlives
the session and the gate, and what a bearer token must still not buy is this
machine's prompts leaving it to a destination whoever holds the token picked.
See
[`RECORD/2026-09-07.the-first-run-has-no-provider.completed.md`](RECORD/2026-09-07.the-first-run-has-no-provider.completed.md).

**Nothing here needs a restart.** A destination **nobody chose** — the file's
`default`, or the mock where there is none, with no `-p` and no flag —
*follows the file* (`Resolved::named`, `/api/settings`' `follows_default`): a
new session that names no provider reads `config.toml` again rather than
inheriting, and saving a new `default` moves the live session at once **while
it has no turns**. One with turns stays where it is and the page says why,
because moving a conversation to another model is a choice with a header of its
own, not a side effect of saving a file. A destination somebody chose is kept,
flags included. **And it is chosen from the chat**: the composer's destination
— one button, `profile · model`, with the same face as the mode's dropup beside
it, so the foot reads as one row of choices — opens a dropup in two columns — the providers, and the models of the one in
focus, which *it* says it serves — and picking one starts a session there when
there are no turns, or moves this one with its history (the starter's
*Continue here*) when there are. Who answers the gate (*Manual* / *Auto*) sits
beside it as a dropup too (`components/dropup.html`), each answer with the line
that says what it means, because a status bar has room for one word and not for
the explanation.

**And so do the three resend rules.** `POST /api/sessions` takes `repeat`,
`prune` and `results` beside the three above — `always`/`once`, `never`/`behind`,
`kept`/`cited`, the header's own words, so the request that started a session and
the recording of it say the arm the same way — each absent meaning *whatever is
in place*, and a value outside the enum refused for the reason a profile is a
name and never a URL. Until this they arrived once at `serve`, as parameters
beside `context_limit` and `evict`, and every session the process ran shared
them: the only way to change one was to restart the binary, which is what made
the project's own differentiator unreachable from the surface anybody uses.
A session that names none **keeps what is in place** rather than resetting to the
flags, which is the destination's rule and not the posture's — a posture is
re-resolved every session because nothing may inherit a container, and these
grant nothing. For the same reason **a resume may move them**, where it may not
move a posture. `/api/settings` reports all three and the fold carries them, so
what a session chose is readable from the page and comparable on a resume:
`retarget_header` gains them as a **fourth term**, because a resume that moved
only the rules and wrote no line would leave the rest of the stream reading as
though it had stayed under the old ones — the posture's own 2026-09-17 bug, one
field along. `None` in the fold is a recording from before format 11, which
cannot be *the same as* the defaults and so gets the line. See
[`RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`](RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md).

**And the machine's own default is `[resend]` in `config.toml`.**

```toml
[resend]
repeat  = "once"     # always | once — `once` is also the code's default
prune   = "never"    # never | behind
results = "kept"     # kept | cited_reads | cited
```

In that file rather than in `localStorage` because it changes the bytes a run
sends, which is a fact about the run and not about the screen somebody reads
from. Every key is optional, and an unnamed one means *this machine states no
position* rather than *this machine chose off* — the same distinction the header
draws between a file that says `always` and one written before the field
existed. `luu chat` and `luu stdio` read it too: it sits beside this machine's
default destination and is the same kind of fact. **A flag beats it, one way
only** — a `bool` flag can only turn a rule on, so `--repeat-once` overrides a
file that says `always` and a file that says `once` cannot be turned off from
the command line. **Rule A is the exception, because its default flipped**: it
has `--no-repeat-once`, so for that one rule a flag beats the file in both
directions and the file is asked only where neither flag was typed. Without it
the flip would have made `always` — the arm every recording before 2026-09-19
was made under — unreachable from any command line.

A session resolves its rules in three levels: **what it asked for**, else **what
the file says**, else **what is already running**. The file is re-read at every
session start, which is `posture_for`'s rule and not the postures list's — a
table the page itself writes has to reach the next session, or saving it does
nothing.

`GET`/`PUT /api/resend` is that table from the browser, under the providers
route's two guards: **403 off loopback**, because a resend rule outlives the
session and a bearer token says who may reach the port rather than what this
machine sends; and **rebuilt from the file on disk**, because the page writes one
table and must not delete the others. The view keeps `file` and `running` apart,
since a session started under a rule nobody wrote down is the ordinary case.

**What the write applies live is narrower than what it writes**, and that is the
two ratchets. Rule A stores nothing, so it moves both ways. Rule B *off* does not
move a running session — the policy gates the decision and the prune line gates
the render, so turning it off would return nothing already behind the line. Rule
C *off* is worse than a no-op: handing tool output back makes the prompt bigger,
and what absorbs bigger is the floor, which drops whole turns and does not come
back. So B and C are applied live only when turned **on**; either turned off is
written to the file and waits for the next session, and the response says which
and why rather than leaving it to be discovered. Where a rule does move, the
stream gains a **second header** through `retarget_header`, so a session reads as
one arm up to a point and another after it.

**A session can be picked back up somewhere else, and the stream says where.**
`POST /api/sessions/{id}/resume` takes the same `{ provider, model }` body, and
the history comes with it — which is the difference between it and starting a
new one. Where the destination differs from what the session's stream already
names, the stream gains a **second `Header` line**: no new variant and no format
bump, because a header is already *the terms these lines were produced under*,
and both readers that fold a stream apply one by overwriting backend and model.
It carries **the session's own `started_at`**, never the moment of the resume,
since every `at_ms` in a stream is relative to **the header above it** — which
within one session's stream is the same number as the first one, and across a
`--record` file holding several sessions is not. The context is
re-folded with the new destination's counter, so a history counted with one
tokenizer is not budgeted with another. The line is checkpointed where it is
written rather than at the next turn, so a session moved and then left alone
keeps the fact. This also closes a hole that predates the choice: a plain resume
inheriting a destination the session never ran on now records that too, instead
of running under a header that names another. See
[`RECORD/2026-09-07.a-second-header.completed.md`](RECORD/2026-09-07.a-second-header.completed.md).

### Record and replay

`luu serve --record <file>` dumps the JSON-lines stream to disk, and the UI can load such a file
instead of a live socket. **The file carries one header per session, not one per process.** Until
2026-09-19 it carried one, written at startup, so a run where somebody pressed *New* — or resumed
onto another posture — produced a file whose first line named the first session's backend, model,
window, counter, eviction, posture and resend rules for turns it did not describe. `create_session`
and the resume route now write the header they were already building for the store's stream into
the recorder as well, and **re-base it**: a header declares what the lines under it are counted
from, so writing one without moving the base leaves the timestamps counted from a moment the file
no longer names. A resume re-bases to the resumed session's *own* start, so the base can move
backwards, and the wall-clock time of any line stays `that header's started_at + at_ms`. See
[`RECORD/2026-09-19.a-header-per-session.completed.md`](RECORD/2026-09-19.a-header-per-session.completed.md). Sessions become replayable offline — useful for bug reports and for
comparing context strategies across runs without re-running inference. What the run *forgot* is in
there too: the recorded fixtures make the two policies visible as what they are — ten small cuts
under `turn` against two deep ones under `block`, over the same twenty prompts, and none at all in
the tasks run, where the fold kept the history under the limit and eviction never fired.

`luu chat --script <file>` runs a file of prompts, one per line, against one shared history. That
is what makes a multi-turn run repeatable: a baseline typed into a browser cannot be re-run, and
two recordings are only comparable when the same task list produced both. The record's header
carries the model, the window, the counter, the eviction policy and the three resend rules for
exactly that reason (`record::FORMAT` 11; the format is 14 since, and `luu count <file>` adds one
up): `repeat`, `prune` and `results`, beside `eviction` and
with the same `None` semantics — a stream written before format 11 does not say, and every one of
those ran under the defaults without recording that it did.
`scripts/tasks/steady-state.txt` is twenty turns of uniform size, long enough to show what a
policy does *after* the first eviction, which is where they stop agreeing. Note that the **mock backend
cannot validate a context strategy** — it does not read the context, so every strategy "wins"
against it; baselines need a real model.

## Inference backend

**Two are built, behind one trait**: `ollama` (`POST /api/chat`, NDJSON) and
`openai` (`POST /chat/completions`, SSE) — the second is not a hosted-API feature,
it is how `llama-server`, vLLM and LM Studio are reached, which is five of the six
machines in [`ROADMAP/2026-09-01/machines.md`](ROADMAP/2026-09-01/machines.md) plus
two hosted endpoints. Binding `llama-cpp-rs` directly, for KV-cache control across
calls without an HTTP server in the way, is still the eventual answer and still
deferred until there is something to measure.

**The window is sent to one of them and cannot be sent to the other**, and the
difference is worth stating where someone will read it before running a
comparison. Ollama takes `options.num_ctx` and truncates silently without it —
the rule AGENTS.md prints in bold. The OpenAI chat-completions API has **no field
for the window at all**: `max_tokens` caps the output, and on `llama-server`,
vLLM and LM Studio the window is what the server was *started* with. So
`--context-limit` is budgeted against and not sent, the CLI says so once before
the run, and the check moves to the response — `usage.prompt_tokens` is already
compared against our own count per turn, and a server serving a smaller window
than we budgeted shows up there.

Which is why **`Chunk::Done` carries `Option<Usage>`**: these servers report no
usage at all unless `stream_options.include_usage` is on the request, and some
report none even then. `None` is *not reported*; zero would claim the server saw
an empty prompt, in exactly the number the budget panel plots against ours. See
[`RECORD/2026-09-01.an-openai-compatible-backend.completed.md`](RECORD/2026-09-01.an-openai-compatible-backend.completed.md).

### Where a model lives

**A provider is a named destination**, kept in `config.toml` in the state
directory: which backend, at which URL, with which model, which key file and
which window the server was started with. `luu chat -p workstation`, and the
flags override it field by field — `-m` moves the model and nothing else, so a
model flag over a local profile asks *that server*, and the answer is its own
404 rather than a redirect.

```toml
default = "local"

[provider.local]
backend = "ollama"
url = "http://127.0.0.1:11434"
model = "qwen2.5-coder:7b"

[provider.workstation]
backend = "openai"
url = "http://192.168.1.40:8080/v1"
model = "qwen2.5-coder-14b"
context-limit = 16384
```

**Not in `luu.toml`, for the reason the session store is not**: the policy file
describes *this project* and is committed with it, while a LAN address, a path
to a key and a model somebody pulled are facts about one machine. `context-limit`
is in the profile because on the OpenAI API the window is a fact about the
*server* — there is no field on a request to send it in — so the number belongs
beside the URL rather than in a flag that gets retyped.

**A remote default has to say `remote = true`, and a local one may not.**
Local-first promises that nothing leaves the machine without the destination
having been typed, and a profile is a destination typed once, weeks ago —
except that `-p workstation` *is* typing it, at the moment of use. The default
is the only shape where nothing was typed, so it is the only one that must have
been written down; a `config.toml` that arrived from elsewhere fails to load if
its default points off the machine. **A box on the LAN counts as off the
machine** — the commitment says *the machine*, not *the building* — and a host
that cannot be parsed counts as off it too. Every run that has somewhere to send
prints where, before it sends: `provider: workstation → openai@192.168.1.40:8080
(remote)`.

**The key is a path, never a value, and its mode is checked.** A key in a config
file is a key in every backup and every `cat` in a bug report; an environment
variable is inherited by every child `run_command` spawns, which is the one place
this repository deliberately runs code it did not write. A file is the only one
of the three whose exposure the program can check, so both secrets — the API key
and `/ws`'s bearer token — are read and checked in the same place. See
[`RECORD/2026-09-07.naming-a-provider.completed.md`](RECORD/2026-09-07.naming-a-provider.completed.md).

**A profile is written and chosen from the browser as well as by hand**: the
configuration modal edits this file on loopback, and a new session names one of
these profiles and a model it serves — see [The first run, and the session that
picks a destination](#the-first-run-and-the-session-that-picks-a-destination).

### A model server luu starts

**An engine is a server luu is allowed to start**: `[engine.<name>]` beside the providers, and a
profile that names it has it started when a session points there, and waited for before anything
is sent to it.

```toml
[engine.gemma]
kind = "llama"          # llama | mlx | ollama
binary = "managed"      # luu's own copy; a path; or absent: found on PATH
port = 8080
model = "ollama:qwen2.5-coder:7b"   # -m, out of what this machine already has
args = ["-c", "8192"]

[provider.gemma]
backend = "openai"
url = "http://127.0.0.1:8080/v1"
engine = "gemma"
```

**The profile keeps its URL**, so the destination a run prints is still one line of the file.
**It is a child of `luu serve`** and dies with it: stopped on SIGINT and SIGTERM, and
`kill_on_drop` for every other way the handle goes. **luu's own copy is an official release by
tag** — `b11236` for llama.cpp, `v0.34.4` for ollama, moved by `version` — downloaded into
`<state dir>/engines/`, checked against the sha256 GitHub publishes for the asset before it is
unpacked, and refused when there is none. **Starting a process is a larger authority than
writing a provider**, so every engine route is loopback-bound, loopback by `Host` as well (a
page that rebinds its DNS to 127.0.0.1 is otherwise same-origin), JSON-only, and from the page an
engine's binary is luu's own copy or a file named `llama-server` or `ollama` — any other path is
written in the file by hand. llama.cpp is the recommended one: 12 MB, no daemon, no model store.
**`mlx` is mlx-serve**, a native binary with a GitHub release and a digest like the other two,
chosen over `mlx_lm.server` by measurement on this machine: 90–92 against 79–81 tok/s decode and
a first token in ~32 ms against ~125 ms, same MLX weights. It also loads GGUF through an embedded
llama.cpp — an ollama blob is handed to it as a `.gguf` link, because it tells the two apart by
extension.

**It is all automatic.** luu's own copy is downloaded the first time a session needs it; the
engine starts then; a session that picks another model restarts a llama.cpp engine on it (it
holds one); and an engine no session sends to is stopped, so a model does not stay in memory
until `serve` exits. `luu chat` and `luu stdio` start it for the length of the run.

**A switch does not wait for the load; the first call does.** Choosing another model answers
as soon as the session points there, and the engine loads behind the answer
(`Supervisor::warm`). The session's backend is wrapped (`engines::Started`) so every call runs
`ensure` first: a turn sent while the model loads waits for it on the server, and an engine that
cannot start reaches the turn as a transport error. `ensure` is one at a time per engine, so the
warm-up and the first turn cannot restart it under each other. **An `ensure` says which phase it
is in** — `stopping` (with `from`, the model the engine was running), `downloading`, `starting`,
then `ready` or `failed` — on one clock for the whole switch, and the page reads it from
`GET /api/engine`. Beside the picker: `starting qwen2.5-coder:14b 3s · qwen2.5-coder:7b stopped`,
then `ready in 4s` for a few seconds, or `could not start` with the reason as its tooltip. What is
happening and for how long comes first, because a chat column is narrow and the end is what gets
cut. When the switch moved to *another* engine, the old one was stopped before the answer came
back and the server has no `from` for it; the page remembers what it left. **A turn that waits
says so where its answer will go** (`waiting for qwen2.5-coder:14b — loading · 3s`), from the same
poll, which every turn start triggers: once per turn (`watchTurnEngine`), not once per change to
the page's state, which during a streamed answer is once per token.
Until 2026-09-29 the switch request waited for the whole load, and with a 14B that was tens of
seconds of a page that looked frozen. See
[`RECORD/2026-09-29.a-switch-that-does-not-wait.completed.md`](RECORD/2026-09-29.a-switch-that-does-not-wait.completed.md).

**The models are the ones already on the machine** (`crate::models`, `GET /api/models`):
ollama's store, llama.cpp's cache, Hugging Face's hub cache and `<state dir>/models`, each
where its tool's own variable says (`OLLAMA_MODELS`, `LLAMA_CACHE`, `HF_HUB_CACHE`). Nothing is
copied. A model is named by a **reference** — `ollama:qwen2.5-coder:7b`,
`huggingface:<org>/<repo>/<file>.gguf`, `huggingface:mlx-community/Qwen3-4B-4bit` — rather than a
path, because a path into ollama's store is a content hash nobody can read; it is shown as
`qwen2.5-coder:7b (ollama)`. **Which engine loads which is one rule**
(`LocalModel::runs_on`): llama.cpp loads GGUF; mlx-serve loads MLX, safetensors and GGUF; ollama
serves its own store. **ollama's MLX models run on ollama only** — they are NVFP4 in
compressed-tensors, which `mlx_lm` refused and mlx-serve loaded and then failed on at the first
matmul. A provider whose engine is llama.cpp or mlx lists every model it can load in the chat's
picker, and the ones it cannot **greyed out, with a warning and the reason as a tooltip**, so a
model on the disk never looks missing.
See [`RECORD/2026-09-28.a-model-server-luu-starts.completed.md`](RECORD/2026-09-28.a-model-server-luu-starts.completed.md).

### Settings

Four sections down a rail: **General, Engines, Models, Sessions**. Engines
come before Models because a provider can name an engine. Sessions is the
resend rules and the authority notes together, each still its own form with its
own save: what a session is rendered under, beyond what was asked. A change of
section is a transition whose end is known before it starts. The chosen section
is mounted invisible and loads, and once its height has settled (unchanged for
90ms, 700ms at the most) the pane goes from the old height to that one in a single
motion while the two sections cross-fade. Sections are mounted per visit, so none
is stale and nothing hidden polls. None of it moves under `prefers-reduced-motion`.

**Every modal opens and closes animated.** It rises a few pixels to full size as
the backdrop darkens, and leaves the same way. The leaving is `modal.js`'s
`closing(id, done)`: every modal's close function goes through it, and ESC is
routed there rather than letting the browser close the dialog at once, so a close
button, the backdrop and ESC all leave alike.
See `RECORD/2026-09-28.components-of-our-own.completed.md`, 2026-09-30.

### The window, when nobody said

A run's window comes from, in order: `--context-limit`, the profile's
`context-limit`, and **the server, when it says** (`WindowFrom::Server`).
`Backend::window` asks `llama-server`'s `/props` (`n_ctx`) and then vLLM's
listing (`max_model_len`). Ollama answers nothing on purpose: there the window
is what luu sends as `num_ctx`, and taking the model's trained length would
ask for a KV cache nobody chose. `serve` asks in the background once a session
points somewhere (an engine waits until it is up). If the session is still
there when the answer comes, the destination takes the window and the stream
gains a header carrying it. `chat` and `stdio` ask once before the first turn
and print `window: 32768, from the server`. See
[`RECORD/2026-09-30.the-window-the-server-says.completed.md`](RECORD/2026-09-30.the-window-the-server-says.completed.md).

### A log for `serve`

`serve` keeps a log in the state directory, `logs/serve.YYYY-MM-DD.log`,
rotated daily, seven kept, filtered by `LUU_LOG` (a `tracing` filter; `info`
for luu's own crates and `warn` for the rest by default), and off with
`--no-log`. It holds every HTTP request as a start and an end with its status
and duration (the path only, never the query, where `?token=` travels), the
sockets opening and closing, each client message by kind, every protocol event
but `token` by type and a fixed set of fields, where a session sends after a
switch, and each engine phase once, when it changes. Never a prompt, a token or a tool's output:
those belong to the session stream. **The writer never blocks**. Lines cross
to `tracing-appender`'s own thread and are dropped rather than waited for,
because a log added to find a freeze must not be able to cause one. **A
watchdog on an OS thread of its own** names every request in flight for more
than five seconds, every five seconds. It also says when a tick that a tokio
task bumps every second stops moving, which tells a lock (the watchdog names
the stuck request) from a blocked runtime (the tick stops). See
[`RECORD/2026-09-30.a-log-for-serve.completed.md`](RECORD/2026-09-30.a-log-for-serve.completed.md).
**A handler's unavoidable filesystem work runs on the blocking pool**
(`blocking`), not on a runtime worker. That includes every read and write of
`config.toml` (`load_config`, `config_path`, `write_config`), the icon-theme
routes, and what `/api/engines` asks the disk. On 2026-09-30 a read of a
`package.json` per installed extension took 147 s, and nothing else was
served while it ran. On the pool the same wait is one slow request. The
session store's SQLite writes are the exception still on the runtime. See
[`RECORD/2026-10-01.a-slow-disk-is-one-slow-request.completed.md`](RECORD/2026-10-01.a-slow-disk-is-one-slow-request.completed.md).

## Persistence

The state directory holds two things: `sessions.db`, and the `config.toml` the
providers above are named in.

**Sessions are stored**, as SQLite (`rusqlite`, with SQLite compiled in so the
store does not depend on the host's system packages). `luu serve` caches its fold
into `sessions.db` in **the state directory** by default — `--store <path>` names
another, `--no-store` keeps the session in memory the way every run did before.
The store is deliberately not beside `luu.toml`: the policy file describes *this
project* and is committed with it, and a session store that travelled with a
checkout would put one project's conversation into every clone of it.

**Where the state directory is, the user decides — once.** `~/.luu` is what a
dotfile-shaped tool does and `~/.config/luu` is what the XDG basedir spec asks
for; which one is right is a fact about somebody's home directory and not about
us, so the first run asks and every run after it finds what that answer created.
Resolved in order: `LUU_HOME` if set, then `~/.luu` if it exists, then
`$XDG_CONFIG_HOME/luu` (default `~/.config/luu`) if it exists, then the question.
**The directory's own existence is the record of the choice** — a setting saying
where the settings live would have to live somewhere, and that somewhere is the
question. Off a terminal it does not ask: it takes the XDG path, says so, and
names `LUU_HOME`, because a prompt nobody can answer is a hang and a hang in
`luu stdio` is a parent process waiting on a pipe. See
[`RECORD/2026-09-04.the-name-and-the-config-dir.completed.md`](RECORD/2026-09-04.the-name-and-the-config-dir.completed.md).

**A row is the fold, whole, and nothing else.** `SessionView` serialised into one
column, with the listing columns derived from `SessionView::summary()` beside it
so a listing does not parse every blob. The normalised alternative — a table per
turn, task and tool call — is a *second definition of the fold*, and the first
time DDL and `api.rs` are changed apart the store and the live server start
disagreeing about a session. `GET /api/sessions` lists the live session and the
stored ones; every read path answers for a stored id as well as for `live`.

**And the stream is beside it**, one row per `RecordLine`, appended at the same
checkpoints in the same transaction (schema 2). That is not the normalised
schema above wearing a hat: those rows would be a second *definition* of a turn,
competing with `api.rs`, while a record line is the account itself and SQL never
looks inside one. It is there because the fold is a cache and until this the
thing it was a cache **of** existed only if somebody had passed `--record` — so
the parity rule could only be checked against files somebody remembered to write,
and a stored session could not be replayed at all. A session stored before schema
2 has none, and that is reported rather than repaired: folding a view back into a
stream would invent line orders and timings the session never had.

**Resuming and multi-session switching are supported.** `SessionStore::resume`
reconstructs the write-side `AgentContext` and turn counter from stored turns and
jobs, validated in `store_parity.rs`. In `serve`, `POST /api/sessions` starts a
clean session checkpointing the active one, `POST /api/sessions/:id/resume`
restores a stored session into the live engine, and `DELETE /api/sessions/:id`
removes past sessions. The web UI header displays stored sessions in a switcher
dropdown alongside a `+ New` button. See
[`RECORD/2026-09-04.session-resume.completed.md`](RECORD/2026-09-04.session-resume.completed.md) and
[`RECORD/2026-09-04.multi-session-in-serve.completed.md`](RECORD/2026-09-04.multi-session-in-serve.completed.md).

**A resumed turn gets its code back by reading it again — the reference, never
the bytes.** Until 2026-09-19 `from_view` rebuilt every turn with an empty
`code_context`, which was the one place in this system where something was
genuinely lost rather than merely not sent: an evicted turn has a tombstone and
a pruned turn has a citation, and a turn whose spans went through a resume was
silently a turn that never had any. What the store holds is the **spec** —
`src/lib.rs:12-40`, on a `grounded` protocol line beside the turn it belongs to,
with the origin (`attached` for what a person typed, `selected` for what
relevance selection chose) — and `from_view` takes a sandbox and reads each one
again. Storing the bytes was rejected on the sandbox and not on staleness: bytes
in a store come back **without being re-read**, so a session resumed under a
narrower posture would get back a file it is no longer allowed to open. A span
this posture cannot read comes back as rule B's citation with the reason in it,
and the resume says which ones out loud rather than restoring four of six in
silence. Two consequences worth stating: the sandbox is the **resume's**, so a
session resumed somewhere else finds none of its spans and comes back all
citations; and because reading again is the only way back, a resume is the one
place item 19's stale-bytes defect is repaired — for one moment, after which the
session goes stale again exactly as a live one does. See
[`RECORD/2026-09-19.fragments-by-reference.completed.md`](RECORD/2026-09-19.fragments-by-reference.completed.md).

- **Whatever the store holds must be reproducible by folding the record.** The
  JSON-lines stream is the account of what happened, and `api::SessionView` already
  folds it; a store that accumulates state the events cannot regenerate is a second
  truth, which is how the static mirror and the live server start disagreeing.
- **Forgetting is an event too.** Built for eviction: an `evicted` line names the
  turns that left, what they were worth, who counted them and which policy cut —
  so a recording says what a session forgot instead of only showing the history
  bucket shrink. The transcript keeps those turns and marks them, because a view
  that agreed with the prompt could no longer show the difference between them.
  The shape is OpenHands' condensation tombstones, read in
  [`RECORD/2026-08-27.cline-openhands.completed.md`](RECORD/2026-08-27.cline-openhands.completed.md);
  the reasoning is in
  [`RECORD/2026-08-31.eviction-tombstones.completed.md`](RECORD/2026-08-31.eviction-tombstones.completed.md).
  Compaction's own tombstone already exists as `task_closed`; pruning tool results
  out of a live turn would need a third, and deliberately has none until it does.
  **And the page reads both cuts, drawn differently on purpose.** Pruning's own
  line is on the trace channel rather than the protocol — what a pruned turn lost
  is a fact about the prompt, not about the conversation — and the panel names the
  turns that paid and what the window saved beside eviction's sentence, while the
  transcript marks a pruned turn *without* striking it through: it is still there
  to be answered from, which is the whole of what rule B buys. Until 2026-09-16
  the page had no consumer for that line at all, so the one rule that gives way
  without losing a turn was the one cut with no mark anywhere on the screen. See
  [`RECORD/2026-09-16.the-panel-reads-the-pruned-lines.completed.md`](RECORD/2026-09-16.the-panel-reads-the-pruned-lines.completed.md).
- A stored turn keeps `code_context` separate from the prompt (per the fusion rule
  above) and its token count together with the counter that produced it. Store the
  fused rendering instead and a resumed session either recomputes everything or sums
  two different units into one bar.
- **The parity is a test, not a promise.** `fold(record) == load(store, id)`, over
  recordings produced by running the binary rather than hand-written, so a store
  that drifted from the fold is a red test instead of a support question. Writes
  happen at checkpoints — a turn ending, a task changing state — so the store is
  allowed to *lag* the record and never to contradict it.
- Compression is deferred until a real session has been measured and is not tens
  of kilobytes of JSON: `zstd` is a dependency, a format decision, and a thing to
  get wrong.

## A session stays on the host that made it

**Sessions do not move between machines, and there is no portal.** Not
"discouraged" and not "an explicit act you may take": there is no mechanism, so
the tree says what this file says.

The reason the question came up was that the model somebody wants is often on a
bigger machine — 32b fits in 48 GB and not in 16 — and the reason it is settled
is that **the model is reachable without moving anything**: `--backend openai
--openai-url http://other-box:8080/v1` has pointed at another machine's
`llama-server` or vLLM since
[`RECORD/2026-09-01.an-openai-compatible-backend.completed.md`](RECORD/2026-09-01.an-openai-compatible-backend.completed.md).
The GPU is a URL; the session, its history, its approvals and its evidence stay
where the person is. And what a session is *about* — this repository — already
moves by the means everyone has: the map is rebuilt from the checkout, fragments
are read from disk at run time, and a plan names paths that would have to be
re-resolved on any other tree anyway.

Three things fall out of the rule rather than being managed by it. **Evidence
never leaves the machine** — a `tool_result` carries the bytes the model was
given, which was the largest thing in this design pointing away from local-first.
**A session cannot fork**, which with an append-only stream is what two copies
of one session are. And **a history cannot be continued under a second
tokenizer**, which would be a budget counted with two rulers.

None of this touches the worker seam: what crosses *there* is the policy and one
tool call, never the session, so a container — local or remote — is unaffected.
A session that has to be read somewhere else is a job for `luu export`, which
folds a recording into the static twin of the read API and moves a recording
rather than a conversation somebody continues.

Argued in
[`RECORD/2026-09-04.sessions-stay-home.completed.md`](RECORD/2026-09-04.sessions-stay-home.completed.md),
against the mechanism that was built first and removed:
[`RECORD/2026-09-04.the-border-and-the-gate.completed.md`](RECORD/2026-09-04.the-border-and-the-gate.completed.md).

## Suggested work order

1. `agent-core`: base types (`Task`, `Context`, `Tool`, `SandboxPolicy`) + inference backend (Ollama/llama.cpp). *Done.*
2. Context manager (the differentiating piece) working in plain CLI, without container or VSCode — to measure and iterate on performance quickly. *History, the budget, whole-turn eviction, block eviction, compaction on task boundaries and the repository map exist; eviction is recorded rather than only applied, and prefix reuse is measured per turn. The reference graph and the ranking exist and are **off by default**: measured against the path-order baseline on this tree they hold two files where the alphabet holds five, because rank order leads with the big central files and the fill rule stops at the first that does not fit. So what is left of the step is the same item with a sharper question — ranking and the fill rule are one decision, not two, and the corpus that would settle them has to be built first, because the existing one defines its control group as "the files a path-ordered map holds". See [`RECORD/2026-09-02.ranking-the-map.completed.md`](RECORD/2026-09-02.ranking-the-map.completed.md).*
3. Agent protocol + `luu serve` + debug web client — early, because it is the instrument used to
   measure step 2. *Done.*
4. Path/command sandbox — in-process checks, then the kernel holding subprocesses. *Done; see the section above. What is still open is per-task policy, which waits on tasks.*
5. Container packaging (level 3), with the level-2 restrictions still applied inside it.
   *The development posture is built and verified live — the worker, the seam,
   the runtime layer, the image, and Landlock ABI v8 active inside Docker
   Desktop on macOS ([`RECORD/2026-09-03.the-container-observed.completed.md`](RECORD/2026-09-03.the-container-observed.completed.md)).
   What is left of the step is the narrowing: egress through the host, and
   `network` per plan. It no longer blocks the exit-code rung or the gate probe's
   command prompts on macOS.*
6. VSCode extension last, once the core is stable — it reuses the protocol from step 3.

The six steps are the *shape* of the work and have not changed. What is being
built next, in what order, and what blocks what is a separate question with a
separate answer: [`ROADMAP/`](ROADMAP/), latest revision. **Federation is
closed**, and the two pieces of it that were built stay because neither was about
moving a session: the wire says its version, so two `luu` builds refuse each
other out loud rather than by misparsing, and an approval is signed with a key no
relay holds, so *who approved* is answered where a bearer token only answers who
could reach the port. Transfer, the probe and the portal are decided against —
see the section above and
[`RECORD/2026-09-04.sessions-stay-home.completed.md`](RECORD/2026-09-04.sessions-stay-home.completed.md);
the argument they were measured against is
[`RECORD/2026-08-31.the-portal-and-the-gate.completed.md`](RECORD/2026-08-31.the-portal-and-the-gate.completed.md).

## Naming

- Project name: **luu**. One name — the crate, the binary, the policy files
  (`luu.toml`, `luu.container.toml`), the container image (`luu-worker`) and the
  repository all say it, so there is nothing to keep in sync.
- It used to be **Loude** with `luu` as a CLI alias, installed as two `[[bin]]`
  entries over one `main`. The alias won: everything a person typed was already
  `luu`, and the old name survived only in the places nobody types — a title, a
  second binary, an image tag, a string inside a signature. A second name that
  appears only where you do not type it is drift, not an alias, so it is gone.
  See [`RECORD/2026-09-04.the-name-and-the-config-dir.completed.md`](RECORD/2026-09-04.the-name-and-the-config-dir.completed.md).
- `RECORD/` still says *Loude* wherever it was written before that date, and its
  links to this file still point at `loude-design.md`. It is append-only: a
  record is what was believed on the day it was written, and repairing the links
  would erase the only evidence that the name ever moved.

## Open questions / next steps

- **`limits` is the last thing in the sandbox a plan cannot narrow.** `files`,
  `writes`, `commands`, `network`, `egress` and now `enforcement` are all
  per-job; what a child may *spend* is still decided once, in `luu.toml`, and a
  plan has no words for a number of seconds or megabytes. Whether it should is
  the open half of
  [`RECORD/2026-09-01.what-the-audit-left.completed.md`](RECORD/2026-09-01.what-the-audit-left.completed.md).
- **The debug UI's gate exposes none of the three narrowing fields.** `network`,
  `egress` and `enforcement` are on the protocol and reachable from a script and
  from any client; the approve button sends files, writes, commands and
  `closes_on`. A UI gap rather than a protocol one, and the same gap for all
  three.
- **A sandbox does not remember which authority *set* its enforcement**, only
  which one is holding the child. A denial inside a job says the job, which is
  true and is not the same as saying the plan is where the value came from.
- **`Sandbox::new` canonicalizes its roots once, at startup**, before any model
  has said anything — so a granted root that is itself a symlink is resolved
  once and trusted afterwards. And `run_command`'s `cwd` is checked and then
  handed to a child as a path; the child is held by Landlock where Landlock
  exists, and where it does not this is the same window one process out.
- **`Sandbox::check_path` still canonicalizes inline**, which is the last
  filesystem call in a turn that a deadline cannot abandon. One bounded syscall
  on a path rather than a read of unknown length, so it is a much smaller window
  than the one the loop's clock closed — and it is the same kind of window.
  `openat2(RESOLVE_BENEATH)` touches that code anyway. Nor does anything bound
  the blocking pool: enough abandoned reads in one session and there are no
  slots left for the ones that would answer.
- Whether `writes` should also bound `run_command`: a child can write whatever
  the task's roots allow, and a plan's `commands` list says nothing about paths.
  Narrower than it was — the child is held to the *task's* roots now — but a
  command is still the widest thing a plan can ask for.
- Who else may close a task: **exit codes are in** — a plan's `closes_on`, matched
  whole, on an exit of 0, and every close says which authority folded it. Tests
  as a distinct rung, and then the judge in shadow mode, are still ahead. Nothing
  counts the two authorities against each other yet, which wants closed tasks in
  quantity and therefore the store.
- Design the concrete GBNF grammar to force valid tool calls with the target model
  (Qwen2.5-Coder), replacing the text parse.
- The CLI has no gate: `luu chat "prompt"` runs one turn with the policy file as
  the standing approval, because a one-shot has no human loop to gate. Whether it
  should grow one, or stay the scripted/one-shot surface it is, is open. It has
  no approver either, for the same reason: its jobs are `approved_by: operator`.
- **Nothing rotates or revokes an approval key.** A compromised one is removed by
  editing `luu.toml` and restarting — the same shape as everything else that file
  decides, and not enough once the fleet is more than the boxes in one room.
- **Nothing signs a *recording*.** A signed approval can be re-verified out of the
  record stream, but a reader that dropped lines is not detected by that. There
  is no relay now, and `luu export` still writes files that travel.
