// The transport layer, kept out of the components on purpose.
//
// Two reasons, both deliberate:
//
// 1. jq79 has no teardown hook yet (RECORD/2026-08-26.web-debug-client.completed.md), so
//    nothing a component owns can be closed when it unmounts. The socket lives
//    here, at module scope, where its lifetime is the page's. That is fine for
//    one page and will not survive per-session components — which is why the
//    hook is on the upstream list rather than crossed off it.
//
// 2. This is the half that will be type-checked. Once the protocol enums are
//    exported with `ts-rs`, this file gets `// @ts-check` against the generated
//    .d.ts; the templates stay untyped. Nothing here should need a DOM node.

import { $reactive } from "@web/vendor/jq79.js"

// What this client speaks, sent on connect so a host that speaks something else
// refuses it out loud rather than by misreading the next message. Kept beside
// `agent_core::protocol::VERSION` and `agent_core::record::FORMAT`: they are
// one number each, and this file is the other half of the pair.
const PROTOCOL = 8
const FORMAT = 19

export const state = $reactive({
  status: "connecting",   // connecting | ready | running | closed | replay
  backend: "",
  model: "",
  protocol: 0,
  // What the host calls this session. An approval is signed against it, so a
  // signature made here does not replay against another host.
  session: null,
  turn: null,
  // `turn` is the session's number for the exchange, which is what an eviction
  // names; `evicted` is the turn whose selection dropped this one, or null, and
  // `pruned` is the turn that took this one's code and left the rest.
  messages: [],           // { id, turn, role, text, task, reason, usage, evicted, pruned }
  budget: null,           // { limit, counter, buckets: [...], backendPrompt }
  prompt: "",             // the exact string sent to the model, last turn
  sending: null,          // a prompt typed here that the server has not answered yet
  prefix: null,           // { shared_bytes, shared_tokens, prompt_tokens } — null on turn 1
  // This turn's tool calls, in order. A call is pushed before it is checked, so
  // one that is still running (or was denied) is visible as itself rather than
  // as nothing happening. Per turn, like the budget beside it.
  // `command` is present for run_command only: { exit_code, signal, stdout,
  // stderr, duration_ms }. Null everywhere else, because an in-process tool has
  // no exit code and a zero would be a lie about a fact that does not exist.
  tools: [],              // [{ step, name, arguments, verdict, error, output, truncated, duration_ms, command }]
  // The model calls after the first one of this turn — the tool round trips.
  // The budget describes the first call only, while the backend's usage is
  // summed over all of them, so the two are comparable only with these added in.
  extraCalls: [],         // [{ step, prompt_tokens, shared_bytes, shared_tokens }]
  // The session's jobs, in the order they were opened. A job with a null plan
  // is a **draft**: its objective is known and its plan is not, and that is the
  // whole of what a draft is. Approving closes one and opens the next.
  jobs: [],               // [{ id, objective, plan, proposed, source, state, summary }]
  tasks: [],              // alias for jobs
  // The one plan on the table, or null. It is **not** a job and has no id — a
  // proposal is offered inside the open draft, and an id is what approval hands
  // out. At most one is ever up, which is what lets the gate address it without
  // naming it. See `RECORD/2026-09-20.every-turn-belongs-to-a-job.completed.md`.
  pending: null,          // { objective, plan, source } | null
  error: null,
  // The configuration modal: what this server resolved (read-only) and the
  // providers file behind it. Both are fetched when it opens rather than kept
  // live — the settings cannot change under a running server, and the file can
  // be edited by hand while one runs, so a stale copy would be the lie.
  settingsOpen: false,
  settings: null,         // GET /api/settings — see serve::Settings
  providers: null,        // GET /api/providers — { path, editable, refused, default, providers, running }
  providersError: null,   // a config.toml that does not load, said where it can be fixed
  // GET /api/resend — { path, editable, refused, file, running, waiting }.
  // `file` is this machine's `[resend]` table and `running` is what the live
  // session is actually rendering under: they can disagree, and a page that
  // showed one as the other would tell somebody their machine is set to
  // something it is not.
  resend: null,
  resendError: null,
  // GET /api/authority — { path, editable, refused, file, running }. `file` is
  // this machine's `[authority]` table and `running` is what the live session
  // is actually telling a draft or a plan about itself. No `waiting`: unlike
  // `resend`, a note has no window state a mid-session move could leave
  // stranded, so a save always takes.
  authority: null,
  authorityError: null,
  // The last thing the server declined to do, and why. Cleared when a turn
  // starts, because by then the answer is on screen.
  refused: null,          // { request, reason, detail }
  // Every turn this page can still show, oldest first, as the inspector will
  // draw it: `{turn, job, budget, prefix, tools, extraCalls, prompt, dropped,
  // usage, reason}`. Filled from `/api/sessions/:id` when a session is opened
  // or resumed — so a turn from before this page existed is here too — and
  // appended to as each live turn ends. Everything in it was already stored and
  // already served; what was missing was a page that kept it.
  history: [],
  // Which turn the inspector is showing, or `null` for the live one. A person
  // reading turn 3 while turn 9 runs keeps reading turn 3: the view moves when
  // they move it.
  selectedTurn: null,
  // `{ path, choosable, running, postures }` — what a new session may be
  // allowed to do, or `null` where nothing has asked yet.
  postures: null,
  // The last cut the window made. Kept beside the budget rather than inside it:
  // the buckets say what the prompt is worth, this says what stopped being in
  // it. Null in a session that never filled its window.
  evicted: null,          // { turn, turns, tokens, counter, policy }
  // The last cut of the other kind. Eviction takes the turn; this takes its
  // code and leaves the turn asked, answered and citable — which is why it
  // arrives on the trace channel rather than the protocol, and why the sentence
  // the panel prints for it is not eviction's. Null where nothing pruned, which
  // is every session run without `--prune-behind`.
  pruned: null,           // { turn, turns, tokens, counter }
  // Whether this session is a recording rather than a server. The status word
  // is not the same question: it says "running" while a recorded turn plays,
  // and a composer that reads the status is enabled over a recording nobody can
  // send into.
  replay: false,
  fixtures: [],           // replay mode only: [{ name, file, about }]
  fixture: "",            // the one being replayed
  sessions: [],           // live server sessions list: [{ id, title, turns, ... }]
  currentSessionId: "live",
})

let socket = null
let traceSocket = null
let backoff = 250
let frame = null
let pending = ""
let nextId = 1

// Messages are replaced, never mutated in place.
//
// jq79 does not wake an `:each` binding when a property of an object inside a
// reactive array is assigned from outside the component — `items[0].text = x`
// renders nothing, `items[0] = {...}` renders. Minimal repro and the upstream
// note are in RECORD/2026-08-26.walking-skeleton.completed.md. Replacing costs one object
// per frame, and `:key` keeps the DOM.
function replaceLast(patch) {
  const index = state.messages.length - 1
  if (index < 0) return null
  const last = state.messages[index]
  if (!last) return null
  state.messages[index] = { ...last, ...patch }
  return last
}

function patchJob(id, patch) {
  state.jobs = state.jobs.map(job => job.id === id ? { ...job, ...patch } : job)
  state.tasks = state.jobs
}
const patchTask = patchJob

/// The transcript as the context sees it: a closed job is one entry, its turns
/// folded behind the summary the model will get from now on.
///
/// Collapsed by default and expandable, which is the pair that matters — the
/// default view is what the prompt now contains, and opening one shows what it
/// no longer does. A debug client that could only show one of those would be
/// hiding the thing this whole design is about.
export function foldTranscript(messages, jobs, expanded) {
  const list = jobs || []
  const closed = new Map(list.filter(j => j.state === "closed").map(j => [j.id, j]))
  const entries = []
  const seen = new Set()

  for (const message of messages) {
    const id = message.job ?? message.task
    const job = closed.get(id)
    if (!job) {
      entries.push({ kind: "message", id: `m${message.id}`, message })
      continue
    }
    if (!seen.has(job.id)) {
      seen.add(job.id)
      entries.push({ kind: "fold", id: `j${job.id}`, job, task: job })
    }
    if (expanded.includes(job.id)) {
      entries.push({ kind: "message", id: `m${message.id}`, message })
    }
  }
  return entries
}

function flush() {
  frame = null
  if (!pending) return
  const last = state.messages[state.messages.length - 1]
  if (last && last.role === "assistant") replaceLast({ text: last.text + pending })
  pending = ""
}

function appendToken(text) {
  pending += text
  if (frame === null) frame = requestAnimationFrame(flush)
}

// The bearer token, when the server was bound off loopback and asked for one.
// The page itself is served without it — a browser navigation cannot carry a
// header — so a person opens `…/?token=…` and the transport takes it from
// there: on the socket as a query parameter, which is the only thing the
// `WebSocket` constructor can do, and on the read side as the header it should
// be everywhere. Null on the loopback default and on a static host, where both
// halves are unchanged.
const token = new URLSearchParams(location.search).get("token")

function url(path) {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:"
  const query = token ? `?token=${encodeURIComponent(token)}` : ""
  return `${scheme}//${location.host}${path}${query}`
}

/// The read side, with the token when there is one.
/// Exported so the workspace panels authenticate the same way the rest of the
/// page does. One place knows how this port is reached; a second copy of this
/// is a second thing to fix when it changes.
export function apiHeaders() {
  return token ? { Authorization: `Bearer ${token}` } : {}
}

function onProtocol(message) {
  // Whatever the server says after a prompt answers it — `turn_started`, a
  // refusal, a plan held at the gate — and whichever it is draws its own
  // thing, so the echo below goes. A token cannot: one only streams in a turn,
  // and a prompt sent during a turn is refused.
  if (message.type !== "token") state.sending = null
  switch (message.type) {
    case "hello":
      state.backend = message.backend
      state.model = message.model
      state.protocol = message.protocol
      state.session = message.session ?? null
      state.turn = message.turn
      state.status = message.turn === null ? idle() : "running"
      if (!isReplay) {
        refreshLiveSession()
        refreshSessionsList()
        // The destination may have moved under the page — a new default the
        // server followed, or another tab's switch.
        refreshSettings()
      }
      break

    case "turn_started": {
      state.turn = message.turn
      state.status = "running"
      state.error = null
      // Whatever was refused, the answer to it is on screen now — with one
      // exception, and it is the refusal a person most needs to read.
      //
      // `not_granted` is not about something that was *stopped*: the approval
      // went through and this is the list of what the policy file would not let
      // it carry — the file the job forgot, the domain nobody may reach, the
      // enforcement nobody may loosen. The turn it describes starts in the same
      // breath the refusal arrives, so clearing it here erased it within a
      // frame, and the only surface that teaches where the policy's floor is
      // taught nothing. Found by the test that drives the gate's three new
      // controls, which is the point of driving a surface. See
      // `RECORD/2026-09-17.the-gate-panel-narrows.completed.md`.
      if (state.refused?.reason !== "not_granted") state.refused = null
      // The job it belongs to travels with the turn, so the transcript can
      // group without replaying the lifecycle to work out what was open.
      const turnJob = message.job ?? message.task ?? null
      state.messages.push({ id: nextId++, turn: message.turn, role: "user", text: message.prompt, job: turnJob, task: turnJob, reason: null, usage: null, evicted: null, pruned: null })
      // `startedAt`, `firstAt` and `endedAt` are this page's clock for a live
      // turn and the stream's `at_ms` for a stored one — two clocks, but a
      // speed is a difference, so either answers it. See `turnSpeed`.
      state.messages.push({ id: nextId++, turn: message.turn, role: "assistant", text: "", job: turnJob, task: turnJob, reason: null, usage: null, evicted: null, pruned: null, startedAt: Date.now(), firstAt: null, endedAt: null })
      state.tools = []
      state.extraCalls = []
      // The two cuts describe something that *happened*, so they belong to the
      // turn that did it and not to the ones after. Uncleared, `keepTurn` copied
      // the last eviction into every turn it kept from then on, and the panel
      // showed turn 5's cut under turn 8's budget — while a reload, which builds
      // the same turns per-turn out of the read API, did not. The budget, the
      // prompt and the prefix beside them are deliberately *not* cleared here:
      // the budget is decided before the call, and blanking it would empty the
      // panel for the length of every turn.
      state.evicted = null
      state.pruned = null
      break
    }

    // The lifecycle. Jobs are replaced rather than mutated, for the same
    // reason messages are: jq79 does not wake an `:each` on a property assigned
    // inside an array element.
    // A turn landed where nothing was open, so a draft opened to hold it.
    case "draft_opened": {
      state.jobs = [...state.jobs, {
        id: message.job,
        objective: message.objective,
        // The field that says this is a draft. Never an empty plan: an empty
        // plan is the *strictest* one there is, and the panel that drew it as
        // "grants nothing" would be right about a job and wrong about this.
        plan: null,
        proposed: null,
        source: null,
        state: "open",
        summary: null,
        closedBy: null,
      }]
      state.tasks = state.jobs
      // The turns it opened to hold. `turn_started` says which job a turn is
      // *in*, and a turn that opens a draft is in one that did not exist when
      // it started — which is why this line carries the turn, and the page had
      // been dropping it. Live, the draft's turns read as belonging to nothing
      // and its fold never appeared; reloaded, the same session got them back
      // from the store with their job on them. Two answers to one question, and
      // the defect is the same shape as the pruning panel's. See
      // `RECORD/2026-09-21.the-alternation-on-the-page.completed.md`.
      if (message.turn != null) {
        state.messages = state.messages.map(entry => entry.turn === message.turn
          ? { ...entry, job: message.job, task: message.job }
          : entry)
      }
      break
    }

    // A plan reached the table, from the model's own suggestion or because
    // somebody asked. It is not a job and goes nowhere near `jobs`.
    case "plan_proposed": {
      // A new gate is a new question, and what the last approval could not
      // carry is not an answer to it.
      state.refused = null
      state.pending = {
        objective: message.objective,
        plan: message.plan,
        // Whether a planning call wrote this plan or the model answered in
        // prose. Null in a recording made before the distinction existed, and
        // then the panel has only emptiness to go on.
        source: message.source ?? null,
      }
      break
    }

    // Turned down, or answered by a prompt — *decline* and *more changes* are
    // the same answer. Nothing closed and nothing folded: the draft it was
    // offered inside is still open and still the live job.
    case "plan_declined":
      state.pending = null
      break

    // Historical, from a recording written before the alternation: the thing it
    // named was a job in a state, and under this shape it is a plan on the
    // table. Replayed as one, so an old recording still draws a gate.
    case "job_proposed":
    case "task_proposed": {
      state.refused = null
      state.pending = {
        objective: message.objective,
        plan: message.plan,
        source: message.source ?? null,
      }
      break
    }

    case "job_rejected":
    case "task_rejected":
      state.pending = null
      break

    case "job_approved":
    case "task_approved": {
      // Approving *is* closing. The draft's own `job_closed` arrives before
      // this one and folds it; this opens the job the plan describes, which is
      // where the id comes from — `message.job` names a job that did not exist
      // until this line.
      const id = message.job ?? message.task
      const offered = state.pending
      state.pending = null
      // A pre-alternation recording approved a job that already existed, so
      // patch it where it does; otherwise this line is the job's birth.
      if (state.jobs.some(job => job.id === id)) {
        patchJob(id, message.plan
          ? { state: "open", plan: message.plan }
          : { state: "open" })
        break
      }
      state.jobs = [...state.jobs, {
        id,
        // The approved plan's own objective. An older recording carries none
        // on this line, and then the proposal it answers is where it was.
        objective: message.objective || (offered ? offered.objective : ""),
        // The plan as approved, which is what the job's sandbox is built from —
        // the person at the gate may have added what the model forgot.
        plan: message.plan ?? null,
        // Kept beside it: the difference between the two is what a person had
        // to add, which is the cost of the gate.
        proposed: offered ? offered.plan : null,
        source: offered ? offered.source : null,
        state: "open",
        summary: null,
        closedBy: null,
      }]
      state.tasks = state.jobs
      break
    }

    // The server declining to do something, which used to be an early return
    // and therefore indistinguishable from a message that never arrived.
    case "refused":
      state.refused = { request: message.request, reason: message.reason, detail: message.detail }
      // Reconnecting cannot fix a version mismatch: the host refuses this
      // client's `hello` and closes, and the retry loop turns that into the
      // word "closed" and nothing else — which is how a page that could not
      // open a session at all went unnoticed for a day. See
      // `RECORD/2026-09-08.a-test-that-clicks-approve.completed.md`.
      if (message.reason === "version") refusedVersion = true
      break

    case "job_closed":
    case "task_closed": {
      // Who folded it. Absent in a recording made before there was more than
      // one authority, and then it was a person: nothing else could close a
      // job when the file was written.
      const id = message.job ?? message.task
      patchJob(id, {
        state: "closed",
        summary: message.summary,
        closedBy: message.by ?? "user",
        // What the fold replaced, counted at the close. `null` in a stream
        // written before record format 9, and the panel says *not recorded*
        // rather than nothing — zero saved is a different claim.
        replaced: message.replaced ?? null,
      })
      break
    }

    case "job_reopened":
    case "task_reopened": {
      // The summary goes with the fold: it is an account of work that is being
      // written again.
      const id = message.job ?? message.task
      patchJob(id, { state: "open", summary: null, closedBy: null, replaced: null })
      break
    }

    // What left the window and stays out. The turns are kept and marked, never
    // removed: a transcript that agreed with the prompt could no longer show
    // the difference between them, which is the one thing this client is for.
    case "evicted": {
      const gone = new Set(message.turns)
      state.messages = state.messages.map(m => gone.has(m.turn) ? { ...m, evicted: message.turn } : m)
      state.evicted = {
        turn: message.turn,
        turns: message.turns,
        tokens: message.tokens,
        counter: message.counter,
        policy: message.policy,
      }
      break
    }

    case "token": {
      const last = state.messages[state.messages.length - 1]
      if (last?.role === "assistant" && last.firstAt == null) replaceLast({ firstAt: Date.now() })
      appendToken(message.text)
      break
    }

    case "ended":
      flush()
      replaceLast({ reason: message.reason, usage: message.usage, endedAt: Date.now() })
      keepTurn(message.turn ?? state.turn, { reason: message.reason, usage: message.usage })
      // The gap between this and what we counted is the chat template, applied
      // where we cannot see it. Reassigned rather than mutated: the panel reads
      // the object, and one write is one update.
      if (state.budget && message.usage) {
        state.budget = { ...state.budget, backendPrompt: message.usage.prompt_tokens }
      }
      state.turn = null
      state.status = idle()
      // A window the server said is learned in the background, after the
      // session starts; the end of a turn is when the page asks again, so the
      // context indicator is not left reading `/ —` over a window luu knows.
      // Not in a replay: a recording has no server to ask, and the static twin
      // answers the question with a 404 in the console.
      if (!isReplay) refreshSettings()
      break

    // Pushed and then replaced rather than mutated: jq79 does not wake an
    // `:each` binding on a property assigned inside an array element.
    case "tool_call":
      state.tools = [...state.tools, {
        step: message.step,
        name: message.name,
        arguments: message.arguments,
        verdict: null,
        error: null,
        output: "",
        truncated: false,
        duration_ms: null,
        command: null,
      }]
      break

    case "tool_result":
      state.tools = state.tools.map(call => call.step === message.step
        ? {
            ...call,
            verdict: message.verdict,
            error: message.error,
            output: message.output,
            truncated: message.truncated,
            duration_ms: message.duration_ms,
            command: message.command ?? null,
          }
        : call)
      break

    case "failed":
      flush()
      replaceLast({ endedAt: Date.now() })
      // A turn that failed is activity worth reading afterwards — often the
      // most worth reading — so it is kept like any other.
      keepTurn(message.turn ?? state.turn, { reason: "failed", error: message.message })
      state.error = message.message
      state.turn = null
      state.status = idle()
      break
  }
}

/// A trace line for a turn this page has already kept. The trace is its own
/// socket, so its lines can land after the protocol's `ended` — against the
/// mock at no delay they do — and `keepTurn` had copied whatever the previous
/// turn left. The kept turn is patched rather than left saying it was never
/// sent anything.
function patchKept(turn, patch) {
  if (turn == null || !state.history.some(entry => entry.turn === turn)) return
  state.history = state.history.map(entry => entry.turn === turn ? { ...entry, ...patch } : entry)
}

function onTrace(message) {
  if (message.type === "prompt") {
    state.prompt = message.text
    patchKept(message.turn, { prompt: message.text })
  }
  // Absent on the first turn of a session: there is no previous prompt, so the
  // panel says so rather than drawing 0%.
  if (message.type === "prefix_reuse") {
    state.prefix = {
      shared_bytes: message.shared_bytes,
      shared_tokens: message.shared_tokens,
      prompt_tokens: message.prompt_tokens,
    }
  }
  if (message.type === "step_call") {
    state.extraCalls = [...state.extraCalls, {
      step: message.step,
      prompt_tokens: message.prompt_tokens,
      shared_bytes: message.shared_bytes,
      shared_tokens: message.shared_tokens,
    }]
  }
  // The other way the window gives way, and the one that leaves the
  // conversation intact: these turns are still asked, still answered and still
  // in the transcript — what left the prompt is their code. Marked rather than
  // struck through for exactly that reason.
  if (message.type === "pruned") {
    const cited = new Set(message.turns)
    state.messages = state.messages.map(m => cited.has(m.turn) ? { ...m, pruned: message.turn } : m)
    state.pruned = {
      turn: message.turn,
      turns: message.turns,
      tokens: message.tokens,
      counter: message.counter,
    }
  }
  if (message.type === "budget") {
    // Arrives before the call now, so a cancelled turn has one too. The
    // backend's own count lands later, on `ended`.
    state.budget = {
      limit: message.limit,
      counter: message.counter,
      buckets: message.buckets,
      backendPrompt: null,
    }
    patchKept(message.turn, { budget: state.budget })
  }
}

let everConnected = false
// Set once the page has given up on a server and taken the recordings instead.
let fellBack = false
// Set when the host refused this client's version. Not retried: the answer
// would be the same every time, and the refusal is the thing to read.
let refusedVersion = false

function open(path, onMessage, assign, greet = false) {
  const ws = new WebSocket(url(path))
  ws.onmessage = event => {
    try {
      onMessage(JSON.parse(event.data))
    } catch (error) {
      // One unreadable frame must not take the page down with it.
      console.error("unparseable frame", error)
    }
  }
  ws.onopen = () => {
    backoff = 250
    everConnected = true
    // Before anything else, because on a port that requires a bearer token the
    // host refuses everything until this arrives. Harmless on loopback, where
    // it is optional.
    if (greet) ws.send(JSON.stringify({ type: "hello", protocol: PROTOCOL, format: FORMAT }))
  }
  ws.onclose = async () => {
    assign(null)
    state.sending = null
    state.status = "closed"

    // The host said what it speaks and it is not this. Retrying would clear
    // the refusal off the screen every few seconds and put it back.
    if (refusedVersion) return

    // A fallback already took the page into replay: there is no server to
    // reconnect to and no second replay to start.
    if (fellBack) return

    // No agent was ever there: this is a static deploy, not a server that
    // restarted. Offer the recorded sessions instead of retrying forever.
    //
    // Both sockets close at once on a static host, so the flag is set *before*
    // the await and not after it. Without that, each of them started its own
    // replay of the same file, and the loser left a turn it had already pushed
    // at the top of the transcript — a user message with an assistant reply
    // that never fills, on every visit to the deployed page.
    if (!everConnected) {
      fellBack = true
      if (await loadFixtures()) {
        socket = traceSocket = null
        return replay(state.fixtures[0].file)
      }
      // Nothing to fall back to after all: a live server that went away.
      fellBack = false
    }

    // The server restarting is the ordinary case during development.
    setTimeout(connect, backoff)
    backoff = Math.min(backoff * 2, 5000)
  }
  return ws
}

/// Replay: the same messages, read from a recorded file instead of a socket.
///
/// This is what makes the UI useful on a static host — GitHub Pages has no
/// agent behind it, and a recorded session is a truer fixture than a hand-made
/// one, because it is a real run of the real protocol.
async function replay(file) {
  // Claimed before the reset, so a replay this one supersedes stops writing
  // into the state we are about to fill: it re-reads this after every await.
  const token = ++replayToken
  reset()
  isReplay = true
  // On the state, not only in this module: the composer asks "is this a
  // recording", and the status word cannot answer it — it says "running" while
  // a recorded turn plays.
  state.replay = true
  state.status = "replay"
  state.fixture = file

  const response = await fetch(file)
  if (!response.ok) {
    state.error = `could not load ${file} (${response.status})`
    return
  }

  const lines = (await response.text()).split("\n").filter(Boolean).map(JSON.parse)

  let previous = 0
  for (const line of lines) {
    if (token !== replayToken) return          // a newer replay superseded this one

    if (line.channel === "header") {
      state.backend = line.backend
      state.model = line.model
      state.protocol = line.protocol
      continue
    }

    // Played back at the pace it was recorded, capped so a session with a long
    // pause in it does not become a long pause here.
    const wait = Math.min(line.at_ms - previous, 400)
    previous = line.at_ms
    if (wait > 0) await new Promise(r => setTimeout(r, wait))
    if (token !== replayToken) return

    if (line.channel === "protocol") onProtocol(line.message)
    if (line.channel === "trace") onTrace(line.message)
  }

  flush()
  state.status = "replay"
}

/// What "not running" means depends on whether there is an agent behind us.
function idle() {
  return isReplay ? "replay" : "ready"
}

/// What the inspector is showing: the turn a person selected, or the live one.
///
/// The live turn is not a special case with its own bindings — it is the same
/// six fields, read out of the fields the socket fills. That is the whole of
/// why the panel can show turn 3: there is one shape, and history is a list of
/// it.
export function panel() {
  if (state.selectedTurn !== null) {
    return state.history.find(entry => entry.turn === state.selectedTurn) || null
  }
  return {
    turn: state.turn,
    job: null,
    budget: state.budget,
    prefix: state.prefix,
    tools: state.tools,
    extraCalls: state.extraCalls,
    prompt: state.prompt,
    dropped: state.evicted,
    cited: state.pruned,
    usage: null,
    reason: null,
    live: true,
  }
}

export function selectTurn(turn) {
  state.selectedTurn = turn === null || turn === "live" ? null : Number(turn)
}

/// Keeps what the panel is showing now, so the next turn does not take it.
///
/// Replaces rather than appends when the turn is already there: a resumed
/// session arrives with its turns from the API, and then runs more of them.
function keepTurn(turn, extra = {}) {
  if (turn === null || turn === undefined) return
  const kept = {
    turn,
    job: null,
    budget: state.budget,
    prefix: state.prefix,
    tools: state.tools,
    extraCalls: state.extraCalls,
    prompt: state.prompt,
    dropped: state.evicted,
    cited: state.pruned,
    usage: null,
    reason: null,
    ...extra,
  }
  state.history = [...state.history.filter(entry => entry.turn !== turn), kept]
    .sort((a, b) => a.turn - b.turn)
}

/// The same shape, out of what the read API stores per turn.
///
/// `dropped` and `cited` gain the turn that made the cut: the stored `Evicted`
/// and `Pruned` name the turns that gave something up and not the selection that
/// took it, which is the turn they are attached to.
///
/// `cited` is the API's name for what this turn pruned, kept here rather than
/// the store's `pruned`, so that the panel reading the two shapes does not have
/// to line up two spellings of one field.
function fromStored(turn) {
  return {
    turn: turn.turn,
    job: turn.job ?? null,
    budget: turn.budget || null,
    prefix: turn.prefix || null,
    tools: turn.tools || [],
    extraCalls: turn.extra_calls || [],
    prompt: turn.prompt_sent || "",
    dropped: turn.dropped ? { ...turn.dropped, turn: turn.turn } : null,
    cited: turn.cited ? { ...turn.cited, turn: turn.turn } : null,
    usage: turn.usage || null,
    reason: turn.reason || null,
    error: turn.error || null,
  }
}

function reset() {
  state.messages = []
  state.history = []
  state.selectedTurn = null
  state.jobs = []
  state.tasks = []
  state.budget = null
  state.tools = []
  state.extraCalls = []
  state.prompt = ""
  state.prefix = null
  state.error = null
  state.refused = null
  state.evicted = null
  state.pruned = null
  state.turn = null
  pending = ""
}

let replayToken = 0
let isReplay = false

export function playFixture(file) {
  if (file) replay(file)
}

/// Asks the read API which sessions exist.
///
/// The same request answers on the live server and on a static host: `luu
/// export` writes this file, and the server serves the same JSON at the same
/// path. Returning false means there is nothing to fall back to, so
/// reconnecting is still right.
async function loadFixtures() {
  try {
    const response = await fetch("./api/sessions.json", { headers: apiHeaders() })
    if (!response.ok) return false
    const sessions = await response.json()
    // Only sessions that ship a recording can be replayed; a live one cannot.
    const replayable = sessions.filter(session => session.record)
    if (!replayable.length) return false
    state.fixtures = replayable.map(session => ({
      name: session.title || session.id,
      file: session.record,
      turns: session.turns,
    }))
    return true
  } catch {
    return false
  }
}

export async function connect() {
  const asked = new URLSearchParams(location.search).get("replay")
  if (asked) {
    await loadFixtures()
    return replay(asked)
  }
  if (socket) return
  socket = open("/ws", onProtocol, s => { socket = s }, true)
  traceSocket = open("/ws/trace", onTrace, s => { traceSocket = s })
}

export function send(text) {
  if (!socket || socket.readyState !== WebSocket.OPEN || !text.trim()) return
  socket.send(JSON.stringify({ type: "prompt", text }))
  // On screen now rather than when `turn_started` comes back: before this the
  // transcript waited on the server, and a server that was slow to answer
  // looked like a composer that had swallowed what was typed.
  state.sending = text
  state.prompt = ""
  state.budget = null
  state.tools = []
  state.extraCalls = []
  state.prefix = null
}

export function cancel() {
  if (!socket || socket.readyState !== WebSocket.OPEN) return
  socket.send(JSON.stringify({ type: "cancel" }))
}

/// Asks for a plan over the draft as it stands.
///
/// The explicit door, beside the model's own suggestion. It is a planning call
/// over the open draft — the refinement is the draft and the plan is what
/// survives it — so it needs no argument: what it plans over is whatever the
/// session is currently in.
export function requestPlan() {
  if (!socket || socket.readyState !== WebSocket.OPEN) return
  socket.send(JSON.stringify({ type: "request_plan" }))
}

/// The other half of the gate, and **neither half names a job any more**.
/// Approving closes the open draft and opens the job the plan describes;
/// declining ends nothing and leaves you in the draft. At most one plan is ever
/// on the table, which is what makes the address unnecessary.
/// **One spelling per frame.** The server accepts `task` as an alias for `job`,
/// so a client written before the rename still works — and a frame carrying
/// *both* is the same field twice, which is a `duplicate field` parse error and
/// a message the server drops on the floor. Sending both was how every approval
/// from this page was silently discarded.
function act(type, id) {
  if (!socket || socket.readyState !== WebSocket.OPEN) return
  socket.send(JSON.stringify({ type, job: id }))
}

/// Approving carries what the person added to the plan, which is the half that
/// makes narrowing survivable: a job may touch what it was approved for, so a
/// plan that forgot a file is widened here rather than rejected and retyped.
/// `closesOn` is the one part of a plan the model is never asked for: a command
/// line whose exit code of 0 folds the job without anyone present. Empty
/// leaves the person as the only authority, which is what every job did before
/// the field existed.
///
/// **One amendment object rather than seven positional arguments.** The last
/// three are the fields this page could not reach until
/// `RECORD/2026-09-17.the-gate-panel-narrows.completed.md`, and every one of
/// them is nullable in a different way — `network` has three states, `egress`
/// is a list merged into the plan's own, `enforcement` is a strictness a person
/// may tighten and not loosen. `approveJob(id, [], [], [], "", null, [], null)`
/// is a call nobody can read.
export function approvePlan(amendment = {}) {
  if (!socket || socket.readyState !== WebSocket.OPEN) return
  const {
    files = [], writes = [], commands = [], closesOn = "",
    // `null` leaves the plan's own answer, which is what an approval that does
    // not care about the network wants to say. `true` asks and is bounded by
    // the policy file; `false` takes it away from a plan that asked.
    network = null,
    egress = [],
    // `"kernel"` or `"best-effort"`, kebab-case because that is how
    // `agent_core::sandbox::Enforcement` is on the wire.
    enforcement = null,
  } = amendment
  socket.send(JSON.stringify({
    // No `job`: an id is what approval hands out, so there is nothing to name
    // until the server answers this. The field exists on the wire only to bind
    // a signature, which this page does not produce.
    type: "approve_plan", files, writes, commands,
    closes_on: closesOn.trim() || null,
    network, egress, enforcement,
  }))
}
export const approveJob = (_job, amendment) => approvePlan(amendment)
export const approveTask = approveJob
export function declinePlan() {
  if (!socket || socket.readyState !== WebSocket.OPEN) return
  socket.send(JSON.stringify({ type: "decline_plan" }))
}
export const rejectJob = declinePlan
export const rejectTask = declinePlan
export const closeJob = job => act("close_job", job)
export const closeTask = closeJob
export const reopenJob = job => act("reopen_job", job)
export const reopenTask = reopenJob

/// Opens the modal.
///
/// **The flag, and nothing else.** Setting it mounts the modal, and the modal
/// mounts whichever section is open — so a fetch here would be a fetch racing
/// the component that draws its result. The section that needs the data awaits
/// it in its own `:setup`, which jq79 runs before the template renders; see
/// `views/settings/models/models.html`, where a reactive statement that rebuilt the draft
/// instead was an effect writing what the component read, and jq79 gave up on
/// it settling after 100 passes.
///
/// A `config.toml` that does not load is still not a reason to show nothing: it
/// is the moment a person most needs to see the file and the message about it,
/// so the error is state rather than a thrown exception.
export function openSettings() {
  state.settingsOpen = true
}

/// What a session may be started under, by name.
///
/// Read-only: a posture is a policy file, and a page that could write one would
/// be a page that could widen its own sandbox. The names come from
/// `config.toml`, read by the server when it started. See
/// `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
export async function loadPostures() {
  try {
    const res = await fetch("./api/postures", { headers: apiHeaders() })
    if (!res.ok) return
    state.postures = await res.json()
  } catch {
    // A static twin has no server behind it, and no session to start either.
  }
}

/// The providers file, without opening anything.
///
/// The session starter needs the list of profiles and is not the modal, which
/// is the whole reason this is not inlined above.
export async function loadProviders() {
  state.providersError = null
  try {
    const providers = await fetch("./api/providers", { headers: apiHeaders() })
    if (providers.ok) {
      state.providers = await providers.json()
    } else {
      state.providers = null
      state.providersError = await providers.text()
    }
  } catch (e) {
    state.providersError = `${e}`
  }
}

/// This machine's `[resend]` table, and what the live session is under.
///
/// Fetched by the section that draws it, like the providers: two questions with
/// two answers, and the modal only mounts the component.
export async function loadResend() {
  state.resendError = null
  try {
    const res = await fetch("./api/resend", { headers: apiHeaders() })
    if (res.ok) {
      state.resend = await res.json()
    } else {
      state.resend = null
      state.resendError = await res.text()
    }
  } catch (e) {
    state.resendError = `${e}`
  }
}

/// Writes `[resend]`, and takes the server's answer as the new truth.
///
/// The answer carries `waiting`: the rules the file now names that the running
/// session was **not** moved onto, because moving them would cost it turns. The
/// server decides that — a page working it out from the two ratchets would be a
/// second copy of a rule that already has one, in the language least able to
/// check it.
export async function saveResend(body) {
  state.resendError = null
  try {
    const res = await fetch("./api/resend", {
      method: "PUT",
      headers: { ...apiHeaders(), "content-type": "application/json" },
      body: JSON.stringify(body),
    })
    if (!res.ok) {
      state.resendError = await res.text()
      return false
    }
    state.resend = await res.json()
    // The settings carry the same three, so a page that refreshed one and not
    // the other would have two panels disagreeing about the live session.
    await refreshSettings()
    return true
  } catch (e) {
    state.resendError = `${e}`
    return false
  }
}

/// This machine's `[authority]` table, and what the live session is telling a
/// draft or a plan. [`loadResend`]'s mirror, one table along.
export async function loadAuthority() {
  state.authorityError = null
  try {
    const res = await fetch("./api/authority", { headers: apiHeaders() })
    if (res.ok) {
      state.authority = await res.json()
    } else {
      state.authority = null
      state.authorityError = await res.text()
    }
  } catch (e) {
    state.authorityError = `${e}`
  }
}

/// Writes `[authority]`, and takes the server's answer as the new truth.
///
/// No `waiting` in the response to read — [`AuthorityView`]'s own reason:
/// a note has no ratchet a save could be unsafe to move. What is saved is
/// what runs, the moment this returns.
export async function saveAuthority(body) {
  state.authorityError = null
  try {
    const res = await fetch("./api/authority", {
      method: "PUT",
      headers: { ...apiHeaders(), "content-type": "application/json" },
      body: JSON.stringify(body),
    })
    if (!res.ok) {
      state.authorityError = await res.text()
      return false
    }
    state.authority = await res.json()
    await refreshSettings()
    return true
  } catch (e) {
    state.authorityError = `${e}`
    return false
  }
}

export function closeSettings() {
  state.settingsOpen = false
}

/// Just the read-only half, for after something changed it.
///
/// Which is a short list and none of it is the file: the settings are fixed for
/// a *session*, and the one thing that starts a new one is `newSession`.
export async function refreshSettings() {
  try {
    const res = await fetch("./api/settings", { headers: apiHeaders() })
    if (res.ok) state.settings = await res.json()
  } catch {
    // Left as it was. A failed refresh is a stale panel, not an empty one.
  }
}

/// Whether this server has nowhere to send, asked once at startup.
///
/// **The answer is the server's.** `unconfigured` is computed where the
/// resolution happens; a page that worked it out from `backend === "mock"`
/// would be a second implementation of a rule that already has one, and would
/// call a deliberate `--backend mock` run unconfigured.
export async function checkConfigured() {
  try {
    const res = await fetch("./api/settings", { headers: apiHeaders() })
    if (!res.ok) return false
    state.settings = await res.json()
    return !!state.settings.unconfigured
  } catch {
    return false
  }
}

/// Writes the file, and takes the server's answer as the new truth.
///
/// The server re-parses what it would write before replacing anything, so a
/// rejection here is the same message a hand-edited file gets — including the
/// one that says a remote default has to declare itself.
export async function saveProviders(body) {
  state.providersError = null
  try {
    const res = await fetch("./api/providers", {
      method: "PUT",
      headers: { ...apiHeaders(), "content-type": "application/json" },
      body: JSON.stringify(body),
    })
    if (!res.ok) {
      // One refusal is answerable rather than only reportable: a default that
      // leaves this machine and has not said so. The server names the host and
      // the caller asks for it to be typed — the declaration is produced by a
      // person, which is the whole content of the rule.
      const refused = await res.json().catch(() => null)
      state.providersError = refused?.message || "the write was refused"
      return { ok: false, declaration: refused?.declaration || null }
    }
    state.providers = await res.json()
    return { ok: true, declaration: null }
  } catch (e) {
    state.providersError = `${e}`
    return false
  }
}

export async function refreshLiveSession() {
  try {
    const res = await fetch("./api/sessions/live", { headers: apiHeaders() })
    if (!res.ok) return
    const view = await res.json()
    reset()
    state.backend = view.backend
    state.model = view.model
    state.jobs = (view.jobs || []).map(j => ({
      id: j.id,
      objective: j.objective,
      plan: j.plan,
      proposed: j.proposed,
      source: j.source,
      state: j.state,
      summary: j.summary,
      closedBy: j.closed_by || null,
      replaced: j.replaced || null,
    }))
    state.tasks = state.jobs

    const msgs = []
    let id = 1
    for (const t of (view.turns || [])) {
      if (t.prompt) {
        msgs.push({
          id: id++,
          turn: t.turn,
          role: "user",
          text: t.prompt,
          job: t.job,
          task: t.job,
          reason: null,
          usage: null,
          // Both marks come from the API and were being thrown away here, so a
          // session reopened after an eviction showed every turn as if it were
          // still in the window — while the inspector's own history, built from
          // the same response a few lines below, had it right.
          evicted: t.evicted_by ?? null,
          pruned: t.pruned_by ?? null,
        })
      }
      if (t.text || (t.tools && t.tools.length)) {
        msgs.push({
          id: id++,
          turn: t.turn,
          role: "assistant",
          text: t.text || "",
          job: t.job,
          task: t.job,
          // Kept rather than dropped, so a reloaded turn has the same meta
          // line — and the same way into its inspection — as a live one.
          reason: t.reason ?? null,
          usage: t.usage || null,
          evicted: t.evicted_by ?? null,
          pruned: t.pruned_by ?? null,
          startedAt: t.started_at_ms ?? null,
          firstAt: t.first_token_at_ms ?? null,
          endedAt: t.ended_at_ms ?? null,
        })
      }
    }
    state.messages = msgs
    // Everything the panel needs for a turn that ended before this page
    // existed. The API answers with it on the same request the transcript is
    // built from, and it used to be dropped on the floor here.
    state.history = (view.turns || []).map(fromStored)
    // And *the newest turn* as the live one, which is what the page reads for
    // "how full is the window": the context indicator, its modal and the
    // inspector's live panel. Those fields are otherwise filled only by trace
    // lines as a turn happens, so after a reload — or a resume — they stayed
    // empty over a session that had turns, and the indicator was disabled
    // with "nothing sent yet".
    const newest = state.history[state.history.length - 1]
    if (newest) {
      state.budget = newest.budget
        ? { ...newest.budget, backendPrompt: newest.usage?.prompt_tokens ?? null }
        : null
      state.prompt = newest.prompt || ""
      state.prefix = newest.prefix
      state.extraCalls = newest.extraCalls || []
    }
    nextId = id
  } catch {
    // Ignore fetch failure
  }
}

export async function refreshSessionsList() {
  try {
    const res = await fetch("./api/sessions", { headers: apiHeaders() })
    if (!res.ok) return
    state.sessions = await res.json()
  } catch {
    // Ignore fetch failure
  }
}

/// Starts a session, optionally somewhere else.
///
/// `{}` — which is what the header's `+ New` sends — means *the destination
/// this server is already pointed at*, exactly as it did before a session
/// could choose one. A `provider` is a profile name out of `config.toml` and
/// never a URL: what may be chosen is bounded by what somebody wrote on this
/// machine.
export async function newSession(choice) {
  const asked =
    choice && (choice.provider || choice.model || choice.posture) ? choice : null
  try {
    const res = await fetch("./api/sessions", {
      method: "POST",
      headers: asked
        ? { ...apiHeaders(), "content-type": "application/json" }
        : apiHeaders(),
      body: asked ? JSON.stringify(asked) : undefined,
    })
    if (!res.ok) {
      const err = await res.text()
      state.error = `Could not create session: ${err}`
      return false
    }
    state.currentSessionId = "live"
    await refreshSessionsList()
    // The destination is the session's now, so what the header shows and what
    // the modal read are both stale the moment this returns.
    await refreshSettings()
    return true
  } catch (e) {
    state.error = `Could not create session: ${e}`
    return false
  }
}

/// What one provider will answer for, and which model to start on there.
///
/// A provider that is not running answers with an empty list and a reason —
/// see `serve::ProviderModels`. That is not an error state in the page either:
/// the field it fills is a suggestion beside an input somebody may type into.
export async function providerModels(name) {
  try {
    const res = await fetch(`./api/providers/${encodeURIComponent(name)}/models`, {
      headers: apiHeaders(),
    })
    if (!res.ok) return { models: [], reason: await res.text(), suggested: null }
    return await res.json()
  } catch (e) {
    return { models: [], reason: `${e}`, suggested: null }
  }
}

/// The posture a stored session ran under, by name, for the resume to say back.
///
/// The server refuses a resume whose posture is not the one the session's jobs
/// were approved against, and it refuses rather than rebuilding it on its own —
/// building one can start a container, and a click on a row in the history is
/// not a request to start a container. So the page says which one it means, and
/// the server still checks that the name resolves to the same three facts. See
/// `RECORD/2026-09-17.what-an-approval-was-granted-under.completed.md`.
///
/// `null` for a session recorded before the posture was in the header, for one
/// that ran on the server's own policy file, and whenever the view cannot be
/// read — all three mean *say nothing*, which is what every resume did before.
async function storedPosture(id) {
  try {
    const res = await fetch(`./api/sessions/${encodeURIComponent(id)}`, { headers: apiHeaders() })
    if (!res.ok) return null
    const view = await res.json()
    return view?.posture?.name ?? null
  } catch {
    return null
  }
}

/// Picks a stored session back up, optionally somewhere else.
///
/// `choice` is `POST /api/sessions`', and means the same thing — a profile out
/// of the file and a model there. What differs is that the history comes along,
/// and that the session's stream gains a header saying where it changed.
///
/// The posture is not part of that choice and never was: it is read off the
/// session being resumed, because a destination is where a session sends and a
/// posture is what it may do.
export async function resumeSession(id, choice) {
  const posture = await storedPosture(id)
  const wanted = {
    ...(choice && (choice.provider || choice.model) ? choice : {}),
    ...(posture ? { posture } : {}),
  }
  const asked = Object.keys(wanted).length ? wanted : null
  try {
    const res = await fetch(`./api/sessions/${encodeURIComponent(id)}/resume`, {
      method: "POST",
      headers: asked
        ? { ...apiHeaders(), "content-type": "application/json" }
        : apiHeaders(),
      body: asked ? JSON.stringify(asked) : undefined,
    })
    if (!res.ok) {
      const err = await res.text()
      state.error = `Could not resume session: ${err}`
      return false
    }
    state.currentSessionId = "live"
    await refreshSessionsList()
    // The turns come back from the server rather than from what this client
    // happened to have: a session resumed elsewhere is the same history, and
    // the page should be reading the one the server just re-folded.
    await refreshLiveSession()
    await refreshSettings()
    return true
  } catch (e) {
    state.error = `Could not resume session: ${e}`
    return false
  }
}

/// Names a session.
///
/// The one thing this page may change about a stored session other than
/// deleting it, and the smallest such thing on purpose: a title is a label a
/// person put on a conversation, and nothing downstream reads it. The server
/// refuses an empty one — a session with no name is a row nobody can pick out
/// of the history. See `serve::rename_session`.
export async function renameSession(id, title) {
  const asked = title.trim()
  if (!asked) return false
  try {
    const res = await fetch(`./api/sessions/${encodeURIComponent(id)}`, {
      method: "PATCH",
      headers: { ...apiHeaders(), "content-type": "application/json" },
      body: JSON.stringify({ title: asked }),
    })
    if (!res.ok) {
      state.error = `Could not rename session: ${await res.text()}`
      return false
    }
    await refreshSessionsList()
    return true
  } catch (e) {
    state.error = `Could not rename session: ${e}`
    return false
  }
}

export async function deleteSession(id) {
  try {
    const res = await fetch(`./api/sessions/${encodeURIComponent(id)}`, {
      method: "DELETE",
      headers: apiHeaders(),
    })
    if (!res.ok) {
      const err = await res.text()
      state.error = `Could not delete session: ${err}`
      return
    }
    await refreshSessionsList()
  } catch (e) {
    state.error = `Could not delete session: ${e}`
  }
}

