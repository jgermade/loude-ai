# luu

A local-first AI agent in Rust that orchestrates calls to models, built for local
inference — 7B–32B models with 8K–32K of context — and free to reach a remote
model when you name one, never as a fallback. The differentiator is context
management: what gets into the prompt, and what earns its place there.

Be concise in your answers. I prefer examples over long text.

[`luu-design.md`](luu-design.md) is the design as it stands. Read it before
proposing anything; a suggestion that contradicts a decision already recorded
there needs to say so and argue with it, not route around it.

## Where things live

- [`luu-design.md`](luu-design.md) — the design as it stands **now**. It is
  rewritten as decisions change, so it always reads as the current answer.
- [`RECORD/`](RECORD/) — dated proposals and the reasoning behind them,
  **append-only**. It is how the current answer was arrived at, including the
  answers that were wrong first.
- [`ROADMAP/`](ROADMAP/) — what is **planned and not yet true**, one directory
  per revision (`ROADMAP/YYYY-MM-DD/`). Rewritable *within* a revision and
  superseded wholesale by a later one, so the roadmap can be corrected without
  the correction erasing what was believed before it.
- `.tmp/` — scratch that is not part of the record: raw run output, a script
  written to drive a measurement, anything that would otherwise land in `/tmp`
  and be unreadable to a check that only trusts the working tree. Gitignored.
  What a run is *worth* still goes in `RECORD/`, evidence included — this is
  only where the disposable half lives while the run is in progress.

The three overlap on purpose and must not be merged. If you only update the
design doc, the reasoning is lost; if you only write a record, nobody can tell
what is true today; if you only keep a roadmap, nobody can tell the plan from the
tree. A decision that changes touches the first two: rewrite the design doc,
append to the record.

The rule that keeps the third honest, because it is the one that rots: **nothing
in `ROADMAP/` is ever the answer to "what is true today."** An item that lands
moves into `luu-design.md` and gets a record; the roadmap entry it came from is
struck through in place, not deleted, so a revision reads as *what we set out to
do and how much of it happened*. A roadmap that quietly loses its misses is a
roadmap nobody can calibrate.

## Plans

Design work that isn't code yet lives in `RECORD/`, one file per proposal, named
`YYYY-MM-DD.<slug>.<state>.md` after the day it was written, where `<state>` is
`WIP` or `completed`.

**The suffix is the record's own state, not the project's.** A record is `WIP`
while it is still waiting on the thing it exists to produce — code it plans, a run
it specifies, a decision it asks for — and becomes `completed` once that has
happened. A *Still open* section does not make a record `WIP`: every record ends
with one by design, and if open threads counted, nothing would ever be complete.
A study, a stock-taking or an argument is `completed` the day it is written,
because writing it is the whole of what it was for.

Renaming is the **only** change a finished record may receive; the content stays
append-only, and the state moves in the filename rather than in an edited line. A
rename breaks every link that points at the file — including the ones in Rust doc
comments — so it updates all of them in the same commit, and `RECORD/` is checked
for dangling links before the commit lands. Write the plan there *before*
touching code — the reasoning is the point, and it's what a future reader (or the
next agent) needs in order to argue with the decision rather than rediscover it. A
plan states the problem, the proposal, what it costs, what was rejected and why,
and what's still open.

**A record argues; a roadmap orders.** They answer different questions and the
split is worth keeping: *why is this the right thing to build* belongs in a dated
record and never changes afterwards, while *what are we building next and in what
order* belongs in `ROADMAP/<revision>/` and is expected to change. So a roadmap
entry is a few lines and a link to the record that argued for it, never a second
copy of the argument — the moment it restates the reasoning it starts drifting
from it, which is the same failure the design doc and the record are split to
avoid. Sequencing, what blocks what, and the Gantt are the roadmap's own content
and belong nowhere else.

**`RECORD/` files are append-only.** Never edit or delete what's already in one,
even to fix something the file gets wrong. A plan is a record of what was believed
on a given day, and rewriting it erases the only evidence of how the thinking
moved. A superseded decision gets a new dated section appended to the bottom (or a
new file, if the whole proposal is being replaced) saying what changed and why. The
mistakes stay where they are, because the reason they were made is usually the most
useful thing in the file.

## Commands

There is a `Makefile` for the four a person types most — `make install`,
`make test`, `make build`, `make up` (the debug UI on 127.0.0.1:7878), plus
`make lint` for what CI runs. Everything below is what those wrap and what
nothing wraps.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets     # CI runs this with RUSTFLAGS=-D warnings
cargo fmt --all --check

cargo run --bin luu -- chat "hola"                    # one turn, mock backend, to stdout
cargo run --bin luu -- chat "hola" --backend ollama   # against a local Ollama
# and against anything OpenAI-compatible: llama-server, vLLM, LM Studio, a
# hosted endpoint, or Ollama's own /v1. The window is the server's — see below.
cargo run --bin luu -- chat "hola" --backend openai \
  --openai-url http://127.0.0.1:8080/v1 --model qwen2.5-coder-7b
# and the same destination written down once, in `config.toml` in the state
# directory: `[provider.<name>]` carries the backend, the URL, the model, the
# key file and the server's window. `-p` names one, `-m` overrides its model,
# and the run says where it is sending before it sends.
cargo run --bin luu -- chat "hola" -p workstation
cargo run --bin luu -- chat "hola" -p workstation -m qwen2.5-coder:14b
# and how its tool calls travel: `fenced` (the default — definitions as text
# in our system prompt, a ```tool block back) or `native` (the server's own
# `tools` field, rendered by its chat template with the tool prompt the model
# was trained on). `tool-calls = "native"` on a profile says it once.
cargo run --bin luu -- chat "read AGENTS.md" -p workstation --tool-calls native
# The profile `default` names must carry `remote = true` when its URL is not
# this machine — a LAN box included — or the file does not load: with no `-p`,
# nobody typed the destination. Naming it is typing it, so every other profile
# is free of the word.
cargo run --bin luu -- serve                          # the debug UI on 127.0.0.1:7878
# sessions are cached in the state directory's sessions.db by default, so a
# restart does not lose the conversation: `--store <path>` moves it,
# `--no-store` turns it off. The first run asks where that directory goes —
# ~/.luu or ~/.config/luu — and LUU_HOME answers it without being asked.
cargo run --bin luu -- serve --no-store
# and a log of what the server did, in the state directory: every request as a
# start and an end, the sockets, the turns, the engines, and a watchdog naming
# whatever is still waiting after five seconds. `LUU_LOG=debug` for more,
# `--no-log` for none. Prompts and output are never in it.
tail -f ~/.config/luu/logs/serve.$(date +%F).log
# The page is three columns, each with a 40px head and foot: an inspector
# (files, git, the context panel), the content, and the chat. Below 1260px it is
# two, and the second column toggles between content and chat from its own head.
# The inspector's and the chat's widths are dragged from their edges (double
# click resets, arrow keys nudge), and kept in localStorage.
# The content column keeps one tab per open thing — a file, a diff, a prompt —
# and a single click opens a preview tab (italic) the next click replaces; a
# double click keeps it. The chat names its session instead of spending a row
# on tabs.
# On a first visit it asks which folder to look at: a subdirectory of the one
# `serve` was started in, which is the ceiling and is not negotiable from the
# browser. Settings has sections down the side — General (theme, editor, layout,
# files, folder), Engines, Models, and Sessions (resend and authority) — and everything in
# General a person can *change* is kept in `localStorage`, because it is a fact
# about the screen rather than about the run. Files is the exception and is read-only: it names the icon theme that
# drew the tree, or says the two shapes are a fallback and names the key below
# that replaces them.
#
# Engines are model servers luu starts: `[engine.<name>]` in config.toml
# (llama-server, mlx-serve or ollama; a binary found here, a path, or luu's own
# copy of an official release, checked against GitHub's sha256), started when a session on
# a profile with `engine = "<name>"` needs it and stopped with `serve`:
#
#   [engine.gemma]
#   kind = "llama"
#   binary = "managed"
#   model = "ollama:qwen2.5-coder:7b"   # any GGUF from ollama, llama.cpp,
#                                       # Hugging Face or <state dir>/models
#
# Downloading luu's copy, starting it, switching its model when the chat picks
# another, and stopping it when no session uses it are all automatic.
#
# Files get VSCode icons if
# `[ui] icon-theme` in config.toml names a theme on this machine (an installed
# extension's directory, or its theme JSON); nothing is vendored, so without
# it the tree draws its own two shapes. Source files are highlighted server-side
# by tree-sitter and follow the light/dark toggle.
#
#   [ui]
#   icon-theme = "~/.vscode/extensions/emmanuelbeziat.vscode-great-icons-3.0.0"
#
# Monaco is the one optional dependency and it is a *node* one, in
# `web/package.json` rather than vendored into the tree: `make install`
# fetches it, the binary serves it from disk, and `General → editor` offers it
# only where it is installed. Without it the page draws every file itself, which
# is the default either way — see
# RECORD/2026-09-16.what-the-debug-ui-does-not-need.completed.md for the
# measurement that decided that.
#
cargo run --bin luu -- stdio                          # protocol over stdin/stdout as NDJSON


# the gate, without a model: the planning call answers with a plan block, then
# the turn answers. Type a prompt and the UI holds it until you approve it.
cargo run --bin luu -- serve --mock-reply '```plan
{"objective": "explain the budget", "steps": ["read the design"], "files": ["luu-design.md"]}
```' --mock-reply 'The budget is split into buckets.'
cargo run --bin luu -- tools                          # the resolved sandbox and the exact prefix block
cargo run --bin luu -- map --map-tokens 1024          # the repository outline that budget resolves to
# and the order it did not take: scores, who references what, and what dropped.
# `--map-rank` makes the map obey it — off by default, and the record says why.
cargo run --bin luu -- map --map-tokens 1024 --explain

# and the other block, the one that is different every turn: the definitions a
# prompt points at, chosen from the same parse and read through the sandbox.
# `--explain` prints every file that scored, not only the ones that fitted.
cargo run --bin luu -- select "which file applies Landlock between fork and exec?" \
  --select-tokens 1024 --explain
# in a turn, off by default — it fills the `code` bucket, which was zero in every
# recording made before it, and is not part of the cached prefix.
cargo run --bin luu -- chat "where is the seam's clock?" --select-tokens 512
# and in the surfaces a person works in, where it is read through the *live
# job's* sandbox: an approved plan narrows what may be chosen exactly as it
# narrows what may be opened, so a narrow plan is a narrow selection.
cargo run --bin luu -- serve --context-limit 32768 --select-tokens 2048
cargo run --bin luu -- stdio --select-tokens 2048
# the coverage probe, which needs no model: 38 questions, one per file, scored
# against scripts/tasks/map-order-probe.key
cargo test -p luu --test select_probe -- --nocapture

# signed approvals: a key, and the signature over one `approve_plan`. The public
# half goes in `[[approvals.key]]`; `[approvals] required = true` then refuses an
# unsigned approval. A wrong one is refused either way. `job` is the id the
# approval will hand out — the session's next — because that is what the
# signature is bound to; an unsigned approval names nothing.
cargo run --bin luu -- key new --out ~/.luu/approval.key --name jgermade
echo '{"type":"approve_plan","job":2,"files":["Cargo.toml"]}' \
  | cargo run --bin luu -- key sign --key ~/.luu/approval.key --session <id> --as jgermade

# what a *session* may do, chosen when it starts: `[posture.<name>]` in the
# state directory's config.toml names a policy file, and `serve` offers the
# names to the page. The posture decides the sandbox and the seam together,
# because one file decides both.
#
#   [posture.container]
#   policy = "luu.container.toml"
#
# A resume may not move it: a destination is where a session sends and a posture
# is what it may do, and its jobs were approved against this one.

# level 3: the same run, with every tool call executed inside a container.
# `--worker direct` is the same seam with no container at all, which is how the
# IPC gets tested where no runtime is installed.
docker build -t luu-worker:dev -f Containerfile .
cargo run --bin luu -- tools --sandbox luu.container.toml
cargo run --bin luu -- chat "hola" --sandbox luu.container.toml
cargo run --bin luu -- tools --worker direct          # the seam, no container
# the whole of level 3 in one script — the image, the handshake, a read, a
# denial and a child process, plus the same read on the host to prove the two
# sides answer the same bytes. CI runs it on every push; a person with a daemon
# runs the same file.
scripts/container-check.sh

# the page itself, clicked: the gate, an amendment, Approve, the tool call and
# the fold — against `luu serve` on the mock, which the spec starts. Node and a
# built binary, which is why it is not in `cargo test`.
make smoke
cd tests/smoke && npx playwright test                        # the static twin
cd tests/smoke && npx playwright test -c gate.config.js      # the live server

# the tool loop end to end without a model: one reply per model call
cargo run --bin luu -- chat "what is in AGENTS.md?" --mock-delay-ms 0 \
  --mock-reply 'looking
```tool
{"name":"read_file","arguments":{"path":"AGENTS.md","max_lines":3}}
```' --mock-reply 'It is the shared instruction file.'

# a real file fused into the turn, read through the sandbox
cargo run --bin luu -- chat "which two commitments does this open with?" \
  --fragment crates/agent-core/src/context.rs:1-15

# a repeatable multi-turn run, which is the only kind worth comparing
cargo run --bin luu -- chat --script scripts/tasks/long-session.txt \
  --context-limit 8192 --tokenizer path/to/tokenizer.json --record before.jsonl

# the same twenty prompts grouped into tasks: the history folds at each `## close`
cargo run --bin luu -- chat --script scripts/tasks/steady-state-tasks.txt \
  --context-limit 1024 --reserve 64 --record tasks.jsonl

# the same run under the other eviction policy — one flag apart, so comparable
cargo run --bin luu -- chat --script scripts/tasks/steady-state.txt \
  --context-limit 512 --evict block --low-water 0.5 --record after.jsonl

# the grounded pair: twenty prompts that are about this repository, with files
# attached, one grouped into tasks. The last five questions attach nothing and
# ask the first fifteen again — in the tasks run those have been folded.
for script in grounded grounded-tasks; do
  cargo run --bin luu -- chat --script scripts/tasks/$script.txt \
    --context-limit 8192 --reserve 512 --record $script.jsonl
done

# the one corpus that edits what it reads, against a copy of its own committed
# tree rather than this one — thirteen turns, four files, and `diverged` lines
# in the recording wherever a prompt ends up carrying one path under two bodies.
# The second arm is the same line with `--select-tokens 1024`.
SCRATCH=$(mktemp -d) && cp -r scripts/tasks/edit-reread/src "$SCRATCH/src"
(cd "$SCRATCH" && luu chat --script "$OLDPWD/scripts/tasks/edit-reread.txt" \
  --allow-write . --context-limit 8192 --record "$SCRATCH/run.jsonl")

./scripts/make-fixtures.sh ./target/debug/luu site/fixtures   # record the replay fixtures
```

`--mock-delay-ms` paces the mock backend, `--mock-reply` scripts what it answers
(repeatable, one per model call, the last repeating), `--cancel-after-ms` exercises
cancelling, and `--record <file>` writes a replayable session from either subcommand.

In `serve`, a prompt with no task open buys a planning call and is then **held,
unrun**, until it is approved or refused in the UI — nothing runs behind the
gate, and a client that sends a prompt anyway gets a `refused` message saying
why: a turn is running, a proposal is pending, the task is not in that state, or
the policy file does not grant part of what was asked. That message is what took
the protocol to v2 (and the record format to 4) — a new variant of a tagged enum
is a change an older reader cannot parse; `evicted` took it to **v3** and the
format to **5** under the same rule. A proposal says **who wrote it**: `source` is
`model` when the planning call emitted a parseable plan block, `prose` when it
answered without one (the ordinary case for a 7B — the proposal is then the ask
itself, declaring nothing), and `written` for a script's `## task:`, where no
planning call happened. Inferring that from an empty plan, which is what the
panel used to do, cannot tell a model that ignored the format from one that
declared an empty list — and which of the two it is decides whether the fix is a
grammar or a sentence. The read side keeps the plan **as proposed** beside the
plan as approved for the same reason: the difference between them is what a
person had to add, which is the cost of the gate. The task lifecycle is a state
machine and every transition is guarded: a proposal cannot be closed, a rejected
plan cannot be reopened. See
[`RECORD/2026-08-30.a-refusal-is-a-message.completed.md`](RECORD/2026-08-30.a-refusal-is-a-message.completed.md). A closed task collapses
in the transcript to the summary the model now gets, expandable to what it no
longer sees.

`serve` binds loopback and answers without authentication, and **any other
address needs `--auth-token-file <PATH>`**: `/ws` carries task approval, so off
loopback that authority is one request away from anyone who can reach the port.
The bind is *refused* without one, before the listener exists, rather than
warned about above a port that is already serving. The token gates `/ws` and
`/api/*` — `Authorization: Bearer <token>`, or `?token=` on `/ws` alone,
because that is all a browser's `WebSocket` constructor can send — and not the
embedded page, so a guarded server is opened at `http://host:7878/?token=…` and
the UI carries it from there. The file's mode is checked: a flag is greppable in
`ps` and an env var is inherited by every child `run_command` spawns.

A script is prompts one per line, `#` comments, and `##` directives for the task
lifecycle — `## task: <objective>`, then `## step:` / `## file:` / `## write:` /
`## command:` for its plan, and `## close`. `## file:` is what the task may
**read** and `## write:` what it may also **change**; a plan that declares no
writes may not write, and a write into a read-only root is refused before the
first turn. `## network:` and `## egress:` say what it may talk to, and
`## enforcement: kernel` is the one directive that narrows **backwards**: a plan
may ask for *more* kernel than the session has and never for less, because the
permissive value here is `best-effort` — so `## enforcement: best-effort` inside
a `kernel` session is refused before the first turn, like a file the policy does
not grant, and the denial that follows a tightened plan names the *job* rather
than telling a person to change a policy file that already says `best-effort`.
`## fragment: <path>[:start-end]` is the other
directive, and it is **not** `## file:`: the plan's files are what the task is
allowed to touch, a fragment is text put into the next prompt.

**The written plan is the approval**: every file it names has to be reachable in
the resolved sandbox and every command allowed by it, or the run stops before the
first turn. **It is also the sandbox for its task** — from `## task:` to
`## close`, and from an approval to a close in `serve`, a turn may touch what the
plan named and nothing else, fragments included, at the level it named: `files`
are granted read, `writes` read-write. The policy file is the outer
bound (a plan cannot grant what it does not) and a plan that names nothing grants
nothing; a denial says which of the two refused. In `serve`, `approve_task`
carries the files and commands the person adds at the gate, checked against the
policy file the same way, which is what keeps an under-specified plan from being
a dead run. See
[`RECORD/2026-08-30.narrowing.completed.md`](RECORD/2026-08-30.narrowing.completed.md). Closing folds the task's turns into a
deterministic summary (the plan, what the tool results reported, and the
fragments the turns were shown, quoted verbatim under a token cap), which is
what the `summaries` bucket in the budget panel plots. A directive it does not
recognise is an error, never a prompt.

The sandbox comes from `luu.toml` — `[sandbox]`, with `paths`, `commands`,
`network`, `enforcement` and `[sandbox.limits]` — and the
`--allow-read/-write/-exec`, `--allow-command`, `--allow-network` and
`--sandbox-enforcement` flags **add** to it for one run. `paths` says what a
child may *reach*; `limits` says what it may **spend**, as `setrlimit` in the
child: `cpu-seconds` and `file-size-mb` are on by default, `memory-mb`
(`RLIMIT_AS`, which a Rust toolchain's address-space reservation makes a
build-breaker) and `processes` (`RLIMIT_NPROC`, which the kernel counts per uid
rather than per process tree) are off until someone types a number. They are
POSIX, not Linux, so they sit above the `linux.rs`/`fallback.rs` split and are
the first thing that has ever held a child on macOS — where the verdict now
reads `rlimits (…)` with the same `missing` instead of "in-process check only". `--max-tool-steps` caps the tool calls one turn may make and
`--no-tools` runs without them. `luu tools` prints what all of that resolved to,
implicit grants included.

**Level 3 exists, in its development posture.** `[worker] runtime` decides where
a tool call actually runs: `host` (this process, the default, and every run this
repository has measured), `direct` (a `luu worker` child with no container), or a
container runtime — `docker`, `podman`, `nerdctl`, Apple's `container`. That
layer builds an *argv* and speaks no runtime's API, which is what makes them
substitutable, and it names the flags that are not uniform rather than assuming
they are. The container's only process **is** `luu worker`, spoken to over stdio,
so its lifetime is the session's and `--rm` plus a closed stdin is the whole of
the cleanup. What crosses the pipe is the **policy**, never the resolved sandbox
— canonical paths are facts about the machine that resolved them — and the
task's `Authority` crosses with it, so a denial from inside still names the plan
that refused. The base is mounted at *its own absolute path*, not `/workspace`,
because a contained run and a host run of the same script have to be diffable
and not merely comparable; `[[worker.paths]]` is the other direction, trees that
exist only inside the image and are therefore never resolved on the host.
`commands` is the image's **manifest**: the worker's handshake reports which of
them the image actually has, and `luu tools` prints the gap under `absent` —
*granted by the policy, absent from the image*, a third failure mode distinct
from a denial and from a kernel that will not hold a child. `direct` isolates
nothing and says so in every line that reports it; it is there so the seam is
testable where no runtime is installed. `Containerfile` builds the image and
`luu.container.toml` is the wide-open posture — a separate file, never a default,
and runs made under it are **not comparable** with runs made without it. See
[`RECORD/2026-09-02.the-worker-and-the-seam.completed.md`](RECORD/2026-09-02.the-worker-and-the-seam.completed.md).

A `run_command` outcome is **structured**: `exit_code`, `signal`, `stdout`,
`stderr` and `duration_ms` are fields on the protocol and in the record, while
`ToolOutcome::render` still hands the model the same short plain text — a 7B
pays for every token of a wrapper it does not read. It is what unblocked the rung
above the user in the closing ladder: a task cannot be closed on an exit code
that only ever existed inside a sentence. `signal` is also how a run says which
limit killed a child (`SIGXCPU`, `SIGXFSZ`). The field is additive — absent for
every in-process tool and in every older recording — so the record format stayed
at 5.

**That rung is built.** A plan carries `closes_on` — one command line — and the
task folds itself when a `run_command` step of its own runs exactly that line and
exits 0. Matched whole (`cargo test` is not `cargo test --no-run`) and on the
code rather than the absence of an error (a child killed by `SIGXCPU` has no exit
code, and that is the limit reporting an unfinished command). **The model is
never asked for it**: `PLANNING` keeps its five keys, so the cached prefix does
not move, and the field arrives in the amendment at the gate — *what would
convince me this is finished* is the person's judgement, not the model's.
`task_closed` carries `by` (`user` or `exit_code`), because a ladder nobody can
count is a ladder nobody can climb. All three are optional fields on existing
types, so the format stayed at 5 and the protocol at v3 — **new variant bumps,
new optional field does not**. It is also unreachable wherever the kernel cannot
hold a child, macOS included, because it sits on `run_command`. See
[`RECORD/2026-09-02.closing-on-an-exit-code.completed.md`](RECORD/2026-09-02.closing-on-an-exit-code.completed.md).

**Sessions survive the process.** `serve` caches its fold into SQLite —
`sessions.db` in the state directory by default (`~/.luu` or `~/.config/luu`,
whichever the first run was told to make; `LUU_HOME` overrides both),
`--store <path>`, `--no-store` to turn it off — and every read path answers for a stored session as well as the live one. A row
is the `SessionView` whole, because a normalised schema is a second definition of
the fold and the day it disagrees with `api.rs` the store lies about a session.
The rule that keeps it honest is a test: `fold(record) == load(store, id)`, over
recordings produced by running the binary. **Resuming is the second fold**, which
had to be argued before it was written: `SessionStore::resume` folds the view
back into a `Context`, and `serve` lists, creates, resumes and deletes stored
sessions. **It takes a sandbox**, because a turn's code comes back by
**reference**: the store holds each span's spec on a `grounded` protocol line
and never its bytes, so a resume *reads it again* through the posture it is
being resumed under. Bytes in a store come back without being re-read, which
would hand a narrowed session a file it may no longer open; a span this posture
cannot read comes back as a citation naming why, and the resume says which ones.
See
[`RECORD/2026-09-19.fragments-by-reference.completed.md`](RECORD/2026-09-19.fragments-by-reference.completed.md),
[`RECORD/2026-09-02.sessions-in-sqlite.completed.md`](RECORD/2026-09-02.sessions-in-sqlite.completed.md),
[`RECORD/2026-09-04.session-resume.completed.md`](RECORD/2026-09-04.session-resume.completed.md)
and
[`RECORD/2026-09-04.multi-session-in-serve.completed.md`](RECORD/2026-09-04.multi-session-in-serve.completed.md).
**The stream is in there too** (schema 2), one row per `RecordLine` appended at
the same checkpoints: the fold is a cache, and the thing it is a cache *of* used
to exist only when somebody passed `--record` — so the parity rule could only be
checked against files somebody remembered to write, and a stored session could
not be replayed at all. A session stored before schema 2 has no stream, and that
is reported rather than repaired: folding a view back into a stream would invent
line orders and timings the session never had.

**A `--record` file carries one header per session, not one per process**, and a
line's `at_ms` is relative to **the header above it** — the same rule the store's
streams follow, and the one `SessionView::from_record`'s *last header wins* was
written for. `create_session` and the resume route write the header they already
build for the stream into the recorder too, and re-base it, because a header
declares what the lines under it are counted from. A resume re-bases to the
resumed session's own start, so the base can move backwards; the wall-clock time
of any line stays `that header's started_at + at_ms`. See
[`RECORD/2026-09-19.a-header-per-session.completed.md`](RECORD/2026-09-19.a-header-per-session.completed.md).

**A session stays on the host that made it.** There is no transfer and no
portal: the model you want on a bigger machine is reached with `--backend openai
--openai-url http://other-box:8080/v1`, the tree moves by git, and what a session
is about is the tree — so what would have crossed is the conversation, at the
price of shipping every `tool_result`'s bytes off the machine. The rule, rather
than a mitigation, is also what makes a fork and a two-tokenizer history
impossible. It says nothing about the worker seam, where the policy and one tool
call cross and the session never does. See
[`RECORD/2026-09-04.sessions-stay-home.completed.md`](RECORD/2026-09-04.sessions-stay-home.completed.md),
and the mechanism it removed:
[`RECORD/2026-09-04.the-border-and-the-gate.completed.md`](RECORD/2026-09-04.the-border-and-the-gate.completed.md).

`--map-tokens N` puts a **repository map** in the prefix: every `.rs` file's
definitions with their signatures, bodies elided, from `tree-sitter`'s own
`TAGS_QUERY`. It goes under the tool definitions and above the history, because
blocks are ordered by how often they are *rewritten* and the map changes only
when the repository does. It is **0 by default** — a map that arrived switched on
would silently change every number in every recording made so far. `luu map`
prints the exact bytes and what they cost.

Files are outlined in **path order** until one does not fit, and the map says how
many it left out. That is not relevance and does not pretend to be: it is the
baseline the reference graph has to beat, and — measured — **still beats it**.
This repository makes the case by itself — the whole outline is 6 327 tokens, **77% of an 8K window**, so at any
budget a real run can afford, most of the repository is missing and the alphabet
picked which part. Two readings to keep straight: the map is paid on *every*
call (870 tokens × 20 turns is +56% on the grounded script's total), and it
**inflates prefix reuse as pure arithmetic** — a bigger constant block raises
shared and total together, so 93.9% → 96.3% is not an improvement and a reuse
number from a run with the map is not comparable to one without it. Numbers and
the two bugs that running it found are in
[`RECORD/2026-08-31.the-repo-map.completed.md`](RECORD/2026-08-31.the-repo-map.completed.md).

`--map-rank` orders the map by **what the rest of the tree depends on** instead:
a reference graph over the same query's `@reference.*` captures, PageRank with a
*uniform* teleport. The uniform part is the whole design — Aider seeds its
ranking with the files in the conversation, and that seed is what would rewrite
the block every turn and cost the map its place in the prefix. Without it a score
depends on nothing but the tree.

It is **off**, and the reason is a measurement rather than caution: at 1024
tokens path order holds five files and the ranking holds two, because rank order
leads with the big central files and the fill rule stops at the first that does
not fit. Two things that finding made clear and neither was in the plan —
**ranking and the fill rule are one decision**, and **`map-probe.txt` cannot
compare two orders**, because its control group is defined as the files a
path-ordered map holds. `luu map --explain` prints the ranking under either
order — each file's score and who references it — so the order the map did not
take can be read beside the one it did. See
[`RECORD/2026-09-02.ranking-the-map.completed.md`](RECORD/2026-09-02.ranking-the-map.completed.md).

`--fragment PATH[:START-END]` on `chat` fuses a real file into the next prompt —
repeatable, 1-based inclusive lines, read **through the sandbox**, and attached
to one turn only. Which turns a file belongs in is what relevance selection
exists to decide later, and attaching it to all of them would answer that now,
wrongly, and pay for it every turn. A path the sandbox refuses is an error, not
a warning: a run that quietly dropped its grounding answers out of the model's
training and looks like it worked. This is what fills the `code` bucket, which
was zero in every recording before the surface existed — and why every script in
`scripts/tasks/` except the grounded pair and `edit-reread.txt` is ungrounded Q&A
that a 7B will answer from training. See
`RECORD/2026-08-27.grounded-fold-probe.completed.md`.

**`edit-reread.txt` is the one that edits what it grounds**, and it is the only
script here whose paths are relative to a tree that is not this one: it runs
against a copy of `scripts/tasks/edit-reread/`, because every prompt that
changes a file changes it for the rest of the session and a corpus that rewrites
its own fixture in place is a corpus that measures the last run. A directory
beside a script and carrying its name is that script's tree — a convention and
not a directive, so `every_script_names_a_file_that_exists` checks its ranges
there while `parse_script` reads the script exactly like every other one. It is
also why that tree is four leaf files and has no `lib.rs`: a fixture inside this
repository is inside this repository's own map, and the module root that was
there first cost `select_probe`'s graph-hop arm two first-place hits. See
`RECORD/2026-09-19.a-corpus-that-edits.completed.md`.

`--context-limit` is the model's window (`0` means unknown: no budget, no
eviction), `--reserve` is what is held back for the answer, `--evict` is how the
history gives way (`turn` drops the minimum, `block` cuts to `--low-water` and
then holds still), and `--tokenizer` points at the model's `tokenizer.json`.
`--repeat-once` renders a span in the **oldest** turn of the window that carries
it instead of in every turn that selected it — **on by default since
2026-09-19**, and oldest rather than newest because that is what leaves the
block above the newest message byte-identical. It is the only one of the three
window rules that has been through a model without moving a verdict, which is
the whole of why it is the one that is on; `--no-repeat-once` is the way back to
the arm every recording before that date was made under. See
[`RECORD/2026-09-07.a-span-is-rendered-once.completed.md`](RECORD/2026-09-07.a-span-is-rendered-once.completed.md)
and [`RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`](RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md).
`--prune-behind` is the second rule and is off: under pressure the oldest
turns give up their spans and keep their exchange — the code becomes a line
citing it, and a turn is dropped whole only when pruning the window is not
enough. The prune line moves to whatever target `--evict` chose, so it is
shallow and frequent under `turn` and deep and rare under `block`, and a run
under it carries `pruned` trace lines naming the turns that gave code up. On
twenty grounded turns it evicts **nothing** where the default drops 13. See
[`RECORD/2026-09-08.prune-behind.completed.md`](RECORD/2026-09-08.prune-behind.completed.md).
A cut says so: the run prints `== evicted turn N` and the recording carries an
`evicted` line naming the turns that left, what they were worth, who counted
them and which policy did it. Over the same twenty prompts at 1024 tokens that
is ten small cuts under `turn`, two deep ones under `block`, and none at all in
the tasks run — the fold kept it under the limit. See
[`RECORD/2026-08-31.eviction-tombstones.completed.md`](RECORD/2026-08-31.eviction-tombstones.completed.md). **Without `--tokenizer` the counts are
`chars/4`**, labelled approximate everywhere they appear — fine for a smoke run,
useless for a comparison, and the numbers say so themselves.

Two of the tests are not unit tests and are the only ones that run the thing:
`crates/agent-core/tests/ollama_wire.rs` puts a stub HTTP server on an ephemeral
port and asserts what `Ollama::stream` actually sends — the window included,
which is the bug that shipped once — and `crates/luu/tests/serve_ws.rs` binds the
server, drives the gate over `/ws`, and checks the read API agrees with what the
socket said. Both run under a plain `cargo test --workspace` in under a second.

The third one is the page, and it is the only thing here that wants node —
`cargo build` still must not:

```sh
cp -r web/. site/ && ./scripts/make-fixtures.sh ./target/debug/luu site/fixtures
cd tests/smoke && npm ci && npx playwright install chromium && npx playwright test
```

It loads the assembled site, replays a recording, clicks through every fixture
in the picker, and fails on any console error other than the socket that cannot
connect on a static host. It runs in the `web` job, after the site is assembled.
It has already earned itself: it found a double replay that had been showing a
ghost turn to every visitor of the deployed page. See
[`RECORD/2026-08-30.tests-that-run-it.completed.md`](RECORD/2026-08-30.tests-that-run-it.completed.md).

CI is `.github/workflows/build.yml` (fmt, clippy, tests, both binaries, the site) on
every push and pull request. `release.yml` is manual: it takes `patch`/`minor`/`major`,
raises the workspace version, tags it, calls `build.yml` at the new commit, publishes
the release and deploys Pages. It has a `dry-run` input — use it before the first
real release.

**Do not edit the UI's `dist`-like output, because there isn't one.** `web/`
is served as it is: `rust-embed` reads it from disk in debug builds and bakes it into
the binary for release. Editing a component costs a reload, not a `cargo build`.

## The two backends, and the one rule that does not transfer

`--backend ollama` and `--backend openai` are both built, behind one trait.
The second is not about hosted APIs: it is how `llama-server`, vLLM and LM Studio
are reached, which is most of the machines in
[`ROADMAP/2026-09-01/machines.md`](ROADMAP/2026-09-01/machines.md).

**`--context-limit` reaches Ollama and cannot reach an OpenAI-compatible
server.** Ollama takes it as `options.num_ctx` and truncates the prompt silently
without it. The chat-completions API has no field for the window at all —
`max_tokens` caps the *output*, and on `llama-server`, vLLM and LM Studio the
window is what the server was started with (`llama-server -c 8192`, vLLM
`--max-model-len 8192`). So `luu` budgets against it, prints a note saying it
cannot send it, and the check moves to the response: `usage.prompt_tokens` per
turn is what actually arrived, and it is already compared against our own count.
**A run against a server started smaller than `--context-limit` is not
comparable to one against Ollama at the same number**, and that gap is the only
place it shows.

Usage itself is the second half of that. These servers stream *no* usage unless
`stream_options.include_usage` is on the request — it always is here — and some
still send none, so `Chunk::Done` carries `Option<Usage>`: `None` is **not
reported**, which the budget panel shows as absent rather than as a prompt of
zero tokens. Ollama always reports counts, so nothing about existing recordings
moved. See
[`RECORD/2026-09-01.an-openai-compatible-backend.completed.md`](RECORD/2026-09-01.an-openai-compatible-backend.completed.md).

`--api-key-file` is the bearer token for a hosted endpoint, a file for the same
reason `--auth-token-file` is one; with no key, no `Authorization` header is sent
at all, which is what a local server wants. The backend's name in a recording is
`openai@<host>`, not `openai`, because a header whose job is to make two runs
comparable cannot call a 7B on a laptop and a hosted frontier model the same
thing.

## Running against a real model (Ollama)

Everything measured so far comes from the mock backend and `chars/4`. That is
legitimate for reuse and token accounting — both are properties of the string we
assemble, decided before the call — and it is **not** legitimate for the question
that decides whether a context strategy is any good: does the task still succeed.
That needs a model. This is the setup for one.

This has been run: `qwen2.5-coder:7b/14b/32b` pulled, tokenizer fetched, the
setup below confirmed correct as written — `num_ctx` reached the server, the
macOS `run_command` denial read exactly as described. What running it actually
found is in [`RECORD/2026-08-27.the-m4-pro-run.completed.md`](RECORD/2026-08-27.the-m4-pro-run.completed.md):
`steady-state`/`steady-state-tasks` carry no grounding, so they cannot answer
whether an answer survives a fold; a tool call inside a turn was invisible to
`prefix_reuse` the same way a planning call had been; and folding's token result
flips with the window — losing narrowly at `--context-limit 1024` (chosen to
force eviction), winning by more than half at the `8192` this recipe uses,
because the baseline never evicts at 8192 and its history compounds unchecked.
The first two are fixed; the grounded pair below is what the first one bought.

**No administrator rights are needed for any of this.** Ollama runs entirely out
of your home directory — the models live in `~/.ollama/models` (`OLLAMA_MODELS`
moves them) and the server is an ordinary user process. What the package manager
buys is a symlink in `/usr/local/bin`, which is the only part that wants `sudo`
and the only part nothing here uses: `luu` talks to a URL.

```sh
# however the binary got onto the machine — unzipped .app, tarball, ~/.local/bin
export OLLAMA_KEEP_ALIVE=30m                 # see below; set it *before* serving
nohup ollama serve > ~/ollama.log 2>&1 &     # detached, listening on 11434
ollama ps                                    # what is loaded, and until when

ollama pull qwen2.5-coder:7b                 # ~4.7 GB, the CLI's default model
ollama pull qwen2.5-coder:14b                # ~9 GB
ollama pull qwen2.5-coder:32b                # ~20 GB — fits 48 GB, much slower

# the tokenizer, which Ollama does not ship: GGUF carries its own vocab, and
# `--tokenizer` wants the HuggingFace file. It must be the tokenizer of the
# model actually being run, which is the entire point of `Counter::Model { id }`.
mkdir -p ~/models/qwen2.5-coder-7b
curl -L -o ~/models/qwen2.5-coder-7b/tokenizer.json \
  https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct/resolve/main/tokenizer.json

cargo run --release --bin luu -- chat "hola" --backend ollama \
  --model qwen2.5-coder:7b --context-limit 8192 \
  --tokenizer ~/models/qwen2.5-coder-7b/tokenizer.json
```

Four things that will otherwise waste a session:

- **The window has to be sent, and it is.** `--context-limit` becomes
  `options.num_ctx` on the request. Ollama's own default is a couple of thousand
  tokens and it truncates the prompt to it *silently*, so without this a run
  budgeting 8k measures a prompt the model never saw. `--context-limit 0` sends
  no option at all and the server keeps its default.
- **A larger window costs memory before it costs anything else.** The KV cache
  grows with `num_ctx` and with the model; 7B at 8–16K is the place to start on
  48 GB, and 32B at 32K is where it stops being comfortable.
- **The prompt cache lives with the loaded model.** If Ollama unloads between
  runs the next first call is cold, and a reuse comparison across runs is
  measuring the unload. `OLLAMA_KEEP_ALIVE` is read by the *server*, so it has to
  be set before `ollama serve` starts, not in the shell that runs `luu`; `ollama
  ps` says what is resident and for how long. Check it between the two halves of
  a comparison, not only before the first.
- **The sandbox is weaker on macOS, and says so.** Landlock and seccomp are
  Linux-only, so `run_command` is *denied* by default with a verdict naming what
  is missing. `--sandbox-enforcement best-effort` runs it anyway, and then the
  allowlist in our own process is all that holds the child — which is exactly
  what the verdict will report. In-process tools (`read_file`, `write_file`,
  `edit_file`, `list_dir`) are unaffected: they never had kernel enforcement.

The runs worth recording first, because they are the ones the mock cannot answer:

```sh
# the pair that asks the question the mock cannot: `grounded.txt` attaches real
# files from this repository and its twin groups the same prompts into tasks, so
# the last five turns — which attach nothing and ask about all of it again — rest
# on the full history in one run and on three summaries in the other.
for script in grounded grounded-tasks; do
  cargo run --release --bin luu -- chat --script scripts/tasks/$script.txt \
    --backend ollama --model qwen2.5-coder:7b \
    --context-limit 8192 --reserve 512 \
    --tokenizer ~/models/qwen2.5-coder-7b/tokenizer.json \
    --record ~/records/$script.jsonl
done

# and the ungrounded pair, which is what the eviction and fold numbers came from
for script in steady-state steady-state-tasks; do
  cargo run --release --bin luu -- chat --script scripts/tasks/$script.txt \
    --backend ollama --model qwen2.5-coder:7b \
    --context-limit 8192 --reserve 512 \
    --tokenizer ~/models/qwen2.5-coder-7b/tokenizer.json \
    --record ~/records/$script.jsonl
done

cargo run --release --bin luu -- serve --record ~/records/live.jsonl   # and look at them
```

**The protocol for that run — the commands, what a right answer to each of the
last five turns contains, how to tell "the summary dropped it" from "the model
ignored it", and the sampling precondition — is
[`RECORD/2026-08-27.grounded-fold-probe.completed.md`](RECORD/2026-08-27.grounded-fold-probe.completed.md).
Read it before running, and append what it says to append.**

**The gate has its own, and it has never been run:**
[`RECORD/2026-08-31.the-gate-probe.completed.md`](RECORD/2026-08-31.the-gate-probe.completed.md) —
fifteen prompts typed through `serve`, what a plan worth approving names for
each, the four ways a denial can happen and how to tell them apart, and the five
numbers to write down. Everything verified for the gate, for narrowing and for
`writes` so far was mock-backed or driven by hand; **no model has ever proposed a
plan that this tree then held it to.** Read it before running that, too — and
approve with the least that will run, because the count of what had to be added
is the measurement.

What to look at, in order: **the last five turns of the grounded pair**, where
the same four questions are answered from a full history in one run and from
three summaries in the other — that comparison is the only thing that says
whether folding loses what the task needed; the gap between our count and
`usage.prompt_tokens` per turn (stable is the chat template; a tooled turn's
round trips are now counted, so they no longer masquerade as it); and whether
reuse behaves as the mock said it would.

## Commits

Keep `Co-Authored-By` when an agent wrote the change — it is conventional
attribution, and readable by anyone.

**Do not add a `Claude-Session:` trailer.** Some agent harnesses append one by
default; this repository is public, and a backlink into a tool nobody reading the
log can open is not worth a permanent line in shared history. Three of the
squashed commits carry one because it was added before anyone weighed that, and
they are left alone rather than rewritten.

This convention was itself written on a branch that was never merged, which is
why it is arriving after the commits it describes rather than before them.

**Every merge into `main` is a squash, and the session trailer is stripped by
hand on the way in.** One commit per pull request, so a branch's own commits
never become ancestors of `main`:

```sh
git merge-base --is-ancestor <branch-head> main   # says no, even when it landed
git diff main..<branch>                           # empty, because it did
```

Ask the diff, not the history. Two things follow, and the first is the one that
bites:

- **Never stack follow-up work on a branch whose pull request merged.** Restart
  it from `main` and put the new commits there. A branch that reads *five commits
  ahead* after a merge is usually four commits of squashed history plus one real
  change, and a pull request built on it asks a reviewer to re-read what they
  already merged.
- **The stripping is this rule being enforced, not a preference about it.** An
  agent that adds a `Claude-Session:` trailer is making hand-work for whoever
  merges, and nothing in `main` can be found by searching for one.

## Design commitments that are easy to erode

These are not style preferences. Each one is load-bearing, and each looks like an
implementation detail from close up:

- **The model never executes anything.** It emits a structured request; the program
  parses it, validates it against the `SandboxPolicy`, and executes real Rust code.
  Any path where model output reaches a shell or the filesystem without passing
  that validation is a bug, however convenient.
- **A plan says what it reads and what it writes, and is held to both.** The
  distinction is the difference between a check that can say no and one that
  only looks like it does: without it every task ran with write access to
  everything it named.
- **The approved plan is the sandbox for its task.** A task boundary is the
  scope permission is granted at, and that is only true if a turn inside a task
  is held to what the task was approved for. Checking the plan against the
  policy and then running with the policy makes the plan a comment.
- **Permission checks live in the code, not in the model behaving well.**
  Canonicalize paths (`std::fs::canonicalize`) before comparing, or a symlink walks
  straight out of the sandbox.
- **A subprocess is the kernel's to hold, not ours.** An in-process check happens
  before the syscall, in a program that then makes the syscall itself; a child
  makes its own, and nothing we wrote is in the way. So `run_command` builds a
  Landlock ruleset and a seccomp filter in the parent and applies them in
  `pre_exec` — and where the kernel cannot, the default is to deny rather than to
  run the child unheld.
- **Nothing may say "sandboxed" without saying by what.** Every verdict carries who
  enforced it, and a partial one carries what is missing. A run the kernel held and
  a run nothing held are not the same run, and afterwards the recording is the only
  thing that could tell them apart.
- **A grant that exists for the child does not answer for a tool.** Allowing a
  command implies read+execute on the system roots, because a program cannot run
  without reading libc — and that reasoning says nothing about `read_file`, so the
  in-process path check ignores those roots.
- **The map is prefix, so it is stable, and it is not ranked yet.** The
  repository outline sits under the tool definitions and does not move for the
  life of a run — that is what makes it cheap on a prompt cache, and it is why
  ranking it (which personalizes it per turn, per task) is a trade to *measure*
  rather than a patch to apply. `#[cfg(test)]` modules are skipped: a module the
  compiler only builds for tests is not the interface the map describes, and in
  this repository the test names were most of the budget. The first file that
  does not fit stops the map rather than being skipped over, so a wider budget
  always shows everything a tighter one did — a map whose contents do not nest
  cannot be compared against itself.
- **Decide what goes in, then render it.** `Context::select` chooses against a
  token budget and the rendering is a pure function of that choice, so every
  token sent is attributable to a bucket. Rendering first and trimming the
  string afterwards loses the attribution and cuts wherever the limit lands —
  sooner or later inside the stable prefix.
- **History is evicted in whole turns, and what leaves the window stays out.**
  Half a turn leaves an answer to a question nobody asked, and a window starting
  on an assistant message makes several chat templates continue instead of
  answering. Eviction is also monotone — `Context` keeps a floor that only moves
  forward — because a turn that comes back rewrites the history from its front,
  and that is the prefix cache's worst case.
- **Forgetting is an event, and it names names.** The selection that moves the
  floor emits `evicted`, on the protocol beside `task_closed` rather than behind
  `--trace`: a fold and an eviction are the two ways history stops being sent,
  and a recording that could show one and not the other says the history bucket
  shrank without saying whether that was the policy or the arithmetic. It names
  the *turns*, which is why `Turn` carries the id the session handed it — the
  position in `Context::turns` is a different number, since a turn that produced
  nothing is never pushed and a planning call is never remembered. The transcript
  keeps an evicted turn and marks it; removing it would make the view agree with
  the prompt and lose the difference between them, which is what the debug client
  is for.
- **Every token count carries which counter produced it.** Two runs measured by
  different counters are not comparable, and nothing else in the system would
  ever say so.
- **Every model call a turn makes is measured, not just the first.** A turn that
  uses a tool is several calls, each carrying the previous result, and the
  backend's `usage.prompt_tokens` is summed over all of them. Measure only the
  first and the two numbers count different things — a 2-token chat-template gap
  reads as 1 962 — and nothing fails. The agent loop announces every call it
  makes before making it; the trace channel measures the ones after the first
  into the same chain. See `RECORD/2026-08-27.the-m4-pro-run.completed.md`.
- **The stable prompt prefix stays byte-identical across calls**, and how much of
  it survived is reported per turn rather than assumed. System text and
  tool definitions are what llama.cpp's prompt cache reuses. The definitions are
  therefore a wire format: tools sorted by name, schemas through `serde_json`'s
  sorted maps, nothing interpolated. `luu tools` prints the exact bytes. Reordering tools,
  re-serializing a schema with different key order, or interpolating a timestamp
  into the system block silently destroys KV reuse — and nothing fails, it just
  gets slower. Treat the prefix as a wire format.
- **One JSON message schema, several transports.** stdio, WebSocket and the record
  format carry the same enums. A field that only makes sense to one client does not
  belong in the protocol; debug traces go behind `--trace`, on their own channel, so
  the protocol doesn't grow a debug half that stdio has to carry forever.
- **`agent-core` knows nothing about the CLI, VSCode or the browser.** That is what
  makes container isolation a packaging question rather than a rewrite.
- **Where a tool call runs is not the loop's business.** The loop asks an
  `Executor`, and there are two: `Tools`, which runs it here, and `Worker`, which
  writes it down a pipe to a `luu worker` inside a container. Adding the
  container touched one parameter of `run_agent_turn` and nothing else, which was
  the test of whether the seam was in the right place. The tool *definitions*
  never cross — they are the cached prefix, and a prefix assembled inside the
  image moves every time the image is rebuilt.
- **`cargo build` must not require node.** The debug UI ships as
  [jq79](https://github.com/jgermade/jq79) — one runtime file plus `.html`
  components, embedded with `rust-embed`. Keep npm out of `build.rs`.

## Before you claim the context manager got better

Context strategies are measured, not argued. A change to compaction, relevance
selection or budget allocation needs numbers from the same model and the same task
set, before and after — token counts per bucket, prompt-cache hit rate, and whether
the task still succeeds. "Fewer tokens" is not a result on its own: dropping the
fragment the model needed is also fewer tokens.

The debug web client exists to produce those numbers — which is why it sits early
in the work order rather than at the end, and why `luu serve --record` writes a
replayable session file. Compare runs, don't recall them.
