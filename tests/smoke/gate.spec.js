// @ts-check
import { expect, test } from "@playwright/test"
import { spawn } from "node:child_process"
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

/**
 * The half of the page the static twin cannot reach: the socket, the gate, an
 * approval, a tool call and the fold. Against a live `luu serve` on the mock —
 * no model, no network, three replies.
 *
 * Why it exists: two bugs in two days were found by a person opening this page
 * and none by a test. See
 * `RECORD/2026-09-08.a-test-that-clicks-approve.completed.md`.
 */

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..")
const PORT = 7897
const BASE = `http://127.0.0.1:${PORT}`

/**
 * The plan the model proposes. It does **not** declare README.md.
 *
 * It **does** declare the network and a domain, which the gate could not show
 * at all until `RECORD/2026-09-17.the-gate-panel-narrows.completed.md`: a plan
 * asking to reach crates.io was approved by a person who was never told it had
 * asked. This session runs under the server's own policy, which grants no
 * network, so it is also the case where what a plan *asks for* and what it will
 * *get* differ — and the panel has to say so rather than report a grant that
 * does not exist.
 */
const PLAN = `\`\`\`plan
{"objective": "say what this repository is",
 "steps": ["read the readme"],
 "files": ["AGENTS.md"],
 "commands": [],
 "network": true,
 "egress": ["crates.io"]}
\`\`\``

/**
 * The call the turn makes once the job is approved — of a path the plan never
 * asked for. It succeeds only if the amendment typed into the browser reached
 * the server and became part of the job's sandbox, which is the bug
 * `RECORD/2026-09-07.a-second-header.completed.md` found by hand. And the
 * amendment grants a *file*, so the job's root is a file: the bug
 * `RECORD/2026-09-08.the-surfaces-first.completed.md` found, asserted where it
 * was found.
 */
const CALL = `\`\`\`tool
{"name":"read_file","arguments":{"path":"README.md","max_lines":1}}
\`\`\``

const ANSWER = "It is luu."

/**
 * The binary under test. Not a skip when it is missing: a skipped test is a
 * test that is not there, and things that are not there is what this file
 * exists to catch.
 */
function binary() {
  const named = process.env.LUU_BIN
  const candidates = named
    ? [resolve(root, named)]
    : [join(root, "target/release/luu"), join(root, "target/debug/luu")]
  const found = candidates.find(path => existsSync(path))
  if (!found) {
    throw new Error(
      `no luu binary at ${candidates.join(" or ")} — run \`cargo build --bin luu\`` +
        " or point LUU_BIN at one",
    )
  }
  return found
}

/** @type {import("node:child_process").ChildProcess | null} */
let server = null
/** @type {string} */
let home

test.beforeAll(async () => {
  // Its own state directory, so the run reads no `config.toml` of the person
  // running it and writes nothing into theirs.
  home = mkdtempSync(join(tmpdir(), "luu-gate-"))
  // And one posture in it, so the starter dialog has something to offer. The
  // policy file is wider than the server's own in the one way the page shows —
  // `network` — so "which one is in force" is answerable from the browser.
  writeFileSync(
    join(home, "wide.toml"),
    '[sandbox]\nnetwork = true\ncommands = ["ls"]\n\n' +
      '[[sandbox.paths]]\npath = "."\naccess = "read-write"\n',
  )
  // A provider as well as the posture: the dialog offers what a session may do
  // beside where it sends, and a session can only be started somewhere this
  // machine has already named.
  writeFileSync(
    join(home, "config.toml"),
    '[provider.here]\nbackend = "mock"\nmodel = "mock"\n\n' +
      `[posture.wide]\npolicy = "${join(home, "wide.toml")}"\n`,
  )
  server = spawn(
    binary(),
    [
      "serve",
      "--bind", `127.0.0.1:${PORT}`,
      "--no-store",
      "--mock-delay-ms", "0",
      "--mock-reply", PLAN,
      "--mock-reply", CALL,
      "--mock-reply", ANSWER,
    ],
    { cwd: root, env: { ...process.env, LUU_HOME: home }, stdio: "pipe" },
  )
  let output = ""
  server.stdout?.on("data", chunk => (output += chunk))
  server.stderr?.on("data", chunk => (output += chunk))

  const deadline = Date.now() + 30_000
  for (;;) {
    if (server.exitCode !== null) {
      throw new Error(`luu serve exited with ${server.exitCode}:\n${output}`)
    }
    try {
      const answer = await fetch(`${BASE}/api/settings`)
      if (answer.ok) return
    } catch {
      // Not listening yet.
    }
    if (Date.now() > deadline) throw new Error(`luu serve never answered:\n${output}`)
    await new Promise(again => setTimeout(again, 200))
  }
})

test.afterAll(() => {
  server?.kill("SIGTERM")
  rmSync(home, { recursive: true, force: true })
})

/**
 * Answers the folder question, which is the first thing a fresh browser gets.
 *
 * The picker covers the window and **has no close**: on a first visit there is
 * nothing behind it to go back to, so the only way out is choosing. Every test
 * below takes the whole workspace, because the tree they assert against is this
 * checkout. See
 * `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
 *
 * Waited for rather than polled: it opens a beat after the socket says hello,
 * and a check that ran before that left it to pop open later over the panel
 * being clicked. Tolerant of it being absent, because Playwright reuses a
 * browser context and the choice is remembered in `localStorage`.
 */
async function chooseFolder(page) {
  const picker = page.locator("dialog.modal").first()
  await picker.waitFor({ state: "visible", timeout: 5_000 }).catch(() => {})
  if (await picker.isVisible()) {
    await picker.locator('button:has-text("Use this folder")').click()
    await expect(picker).toBeHidden()
  }
}

test("a prompt is planned, amended, approved, run and folded", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  // The logo, which is also the control that goes back to the chat. It is in
  // the inspector's head since the page-wide `<header>` came out — that header
  // was a strip of other columns' controls. See
  // `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
  await expect(page.locator(".col.inspector .logo")).toHaveText("luu")

  // The first thing a fresh browser is asked: which folder.
  await chooseFolder(page)

  // The first run's other half: nothing in `config.toml` says where this
  // machine sends. It is said in Settings → Models rather than by a modal that
  // opens itself, because on a first visit that would be the second dialog
  // over a folder question that cannot be dismissed.
  await page.click('.inspector .col-foot button[title="Settings"]')
  await page.click('.modal .rail button:has-text("Models")')
  await expect(page.locator(".first-run")).toBeVisible()
  await page.locator(".modal-head button.link", { hasText: "close" }).click()
  await expect(page.locator("dialog.modal")).toHaveCount(0)

  // The prompt runs, in the draft it opens, and the model's answer carries a
  // plan block — so a plan reaches the gate as a suggestion.
  const composer = page.locator(".composer input")
  await expect(composer).toBeEnabled()
  await composer.fill("what is this repository?")
  await page.click('.composer button[type="submit"]')

  const gate = page.locator(".gate")
  await expect(gate).toBeVisible({ timeout: 30_000 })
  await expect(gate.locator(".objective")).toHaveText("say what this repository is")
  await expect(gate.locator(".plan li")).toHaveText(["read the readme"])
  await expect(gate.locator("p", { hasText: "reads:" })).toContainText("AGENTS.md")
  // What it asks to reach, and what it will actually get: this session took the
  // server's own policy file, which grants no network at all.
  const reach = gate.locator("p", { hasText: "network: yes" })
  await expect(reach).toContainText("egress: crates.io")
  await expect(reach).toContainText("the session's policy denies it")
  // The other term of the decision, and the one the panel could not show until
  // `RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`: what the
  // turn in front of the gate already has. Approving is a decision about what to
  // *add*, and a panel that states one term is a panel that reads as a yes to a
  // list. The floor grants no writes by construction, so that is a sentence.
  const floor = gate.locator("p.floor")
  await expect(floor).toContainText("unapproved, a turn reads")
  await expect(floor).toContainText("writes nothing")
  // The draft is open behind it, holding the turn that opened it.
  await expect(page.locator("article.assistant")).not.toHaveCount(0)

  // What the model forgot, added by hand. The plan is the job's sandbox, so
  // this is the only moment the file the turn will actually read can get in.
  await page.fill('.amend input[placeholder*="the plan forgot"]', "README.md")
  await page.click('.amend button:has-text("add read")')
  await expect(page.locator("p", { hasText: "adding reads:" })).toContainText("README.md")

  await page.click('.gate-buttons button:has-text("Approve")')
  await expect(gate).toBeHidden({ timeout: 15_000 })

  // Approving closes the draft and opens the job; the work starts on the next
  // prompt, which lands inside it.
  await expect(composer).toBeEnabled()
  await composer.fill("go on then")
  await page.click('.composer button[type="submit"]')

  // The call the plan never declared, allowed by the amendment and answered.
  const call = page.locator(".timeline li").first()
  await expect(call).toBeVisible({ timeout: 30_000 })
  await expect(call.locator(".call b")).toHaveText("read_file")
  await expect(call.locator(".verdict .ok")).toHaveText("allowed")
  await expect(call.locator(".verdict")).toContainText("README.md")
  // A read that was granted and then failed — `ENOTDIR` on a file root was
  // exactly that — reads as an error line here, and as no bytes coming back.
  await expect(page.locator(".timeline .err")).toHaveCount(0)
  await expect(call.locator(".held")).toContainText("bytes back")

  // `toContainText`, not `toHaveText`: a turn that used a tool is one assistant
  // message carrying every round trip it made — the fenced call it emitted and
  // then the answer.
  await expect(page.locator("article.assistant .text").last()).toContainText(ANSWER)
  // And the call as a card where the model wrote it, not as the fence it typed.
  // See `RECORD/2026-09-29.a-switch-that-does-not-wait.completed.md`.
  const card = page.locator("article.assistant .call").last()
  await expect(card.locator(".name")).toHaveText("read_file")
  await expect(card.locator(".subject")).toHaveText("README.md")
  await expect(card.locator(".status")).toHaveText("done")
  await expect(page.locator("article.assistant", { hasText: "```tool" })).toHaveCount(0)

  // The meta line opens the turn: the prompt it was sent, cut at its roles,
  // the answer as the model wrote it, and the calls. See
  // `RECORD/2026-09-30.the-foot-and-the-turn.completed.md`.
  const meta = page.locator("article.assistant button.meta").last()
  // `→ sent ← received - 🕑 first → last - speed`
  await expect(meta).toContainText("→")
  await expect(meta).toContainText("←")
  await meta.click()
  const turn = page.getByRole("dialog", { name: /^Turn / })
  await expect(turn).toBeVisible()
  // Grouped by what each piece is, each in its bucket's colour: the tool
  // definitions under tools, the question under prompt.
  await expect(turn.locator('.group[data-bucket="tools"] pre')).toContainText("# Tools")
  await expect(turn.locator('.group[data-bucket="prompt"] pre').last()).toHaveText("go on then")
  await expect(turn.locator(".group header b").first()).toHaveText("system")
  await turn.locator(".tabs button", { hasText: "Answer" }).click()
  await expect(turn.locator("pre.raw")).toContainText("```tool")
  await expect(turn.locator("pre.raw")).toContainText(ANSWER)
  await turn.locator(".tabs button", { hasText: "Tools" }).click()
  await expect(turn.locator(".role b")).toHaveText("read_file")
  await turn.locator(".modal-head button", { hasText: "close" }).click()
  await expect(turn).toBeHidden()

  // The context so far, from the indicator beside the composer: the same
  // groups, about the newest turn.
  const ctx = page.locator("footer.chat-foot button.ctx")
  await expect(ctx).toBeEnabled()
  await ctx.click()
  const context = page.getByRole("dialog", { name: "Context" })
  await expect(context).toBeVisible()
  await expect(context.locator('.group[data-bucket="prompt"] pre').last()).toHaveText("go on then")
  await context.locator(".modal-head button", { hasText: "close" }).click()
  await expect(context).toBeHidden()

  // And after a reload, which rebuilds the session from the server: the
  // indicator used to come back disabled over a session that had turns,
  // because only live trace lines ever filled what it reads.
  await page.reload()
  await expect(ctx).toBeEnabled({ timeout: 15_000 })
  await ctx.click()
  await expect(context.locator('.group[data-bucket="prompt"] pre').last()).toHaveText("go on then")
  await context.locator(".modal-head button", { hasText: "close" }).click()

  // One entry where its turns were.
  const live = page.locator(".live-job")
  await expect(live).toBeVisible()
  await live.locator("button", { hasText: "close & fold" }).click()
  await expect(live).toBeHidden({ timeout: 15_000 })
  await expect(page.locator(".fold .summary").first()).toBeVisible()

  // The transcript now says which half of the alternation each fold was, and
  // why it ended. Before
  // `RECORD/2026-09-21.the-alternation-on-the-page.completed.md` both folds
  // read "job N · objective" and a reader could not tell exploration nobody
  // approved from work somebody signed for.
  const folds = page.locator(".chat .fold")
  await expect(folds).toHaveCount(2)
  await expect(folds.first()).toContainText("draft 1")
  await expect(folds.first()).toContainText("closed by approving its plan")
  await expect(folds.last()).toContainText("job 2")

  // And `reopen` is offered only where the server would take it: the last job
  // and no other. It is on the job that was just folded, and not on the draft
  // the approval closed before it — which the route refuses, and used to
  // refuse by saying the job was not closed.
  await expect(folds.first().locator("button", { hasText: "reopen" })).toHaveCount(0)
  await expect(folds.last().locator("button", { hasText: "reopen" })).toHaveCount(1)

  // The fold, with what it cost: the compaction log is the fourth of the four
  // panels the design says justify building this client, and the numbers in it
  // are counted at the close because they are only true then. See
  // `RECORD/2026-09-08.what-a-fold-writes-down.completed.md`.
  const fold = page.locator(".folds li").first()
  await expect(fold).toBeVisible()
  // `job 2`: job 1 is the draft the approval folded, and this is the job it
  // opened — approving *is* closing, so a session that got this far has two.
  await expect(fold).toContainText("job 2")
  // Both numbers come from the close and neither is counted in the page, so
  // this asserts digits rather than the word: an empty span either side would
  // still have read as "tokens of history … saved".
  await expect(fold).toContainText(/\d+ tokens of history →\s+\d+ in the line/)
  await expect(fold).toContainText(/-?\d+ saved/)
  await expect(fold).not.toContainText("not recorded")

  // And the two turns are still readable, without a reload: the panel keeps
  // what it was showing instead of resetting when the next turn starts.
  await expect(page.locator(".turn-picker select option")).toHaveText([
    "live", "turn 1", "turn 2",
  ])

  expect(errors, "the page logged errors").toEqual([])
})

/**
 * The panel over a turn that is no longer the live one — from the stream while
 * the page stays open, and from the read API after it is reloaded. Runs on the
 * session the test above left behind: turn 1 is the planning call, turn 2 is
 * the approved run with the tool call in it.
 *
 * See `RECORD/2026-09-08.the-panel-keeps-the-turn.completed.md`.
 */
test("the inspector can be pointed at a turn that has ended", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  // Reloaded, so the history comes from `/api/sessions/live` rather than from
  // messages this page watched go by. Both fill the same panel and only one of
  // them survives a refresh.
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  const picker = page.locator(".turn-picker select")
  await expect(picker.locator("option")).toHaveText(["live", "turn 1", "turn 2"])

  // The planning call: a prompt was sent, and it used no tools.
  await picker.selectOption("1")
  await expect(page.locator(".reading")).toContainText("Turn 1 as it was sent")
  await expect(page.locator(".inspector pre")).toContainText("You are luu")
  await expect(page.locator(".timeline li")).toHaveCount(0)

  // The approved run: the call the amendment made possible, still there.
  await picker.selectOption("2")
  await expect(page.locator(".reading")).toContainText("Turn 2 as it was sent")
  await expect(page.locator(".timeline li")).toHaveCount(1)
  await expect(page.locator(".timeline .call b")).toHaveText("read_file")
  await expect(page.locator(".timeline .verdict")).toContainText("README.md")
  // Every bucket of that turn's budget, not the empty one a reset leaves.
  await expect(page.locator(".legend li")).not.toHaveCount(0)

  await page.locator(".turn-picker button", { hasText: "back to live" }).click()
  await expect(page.locator(".reading")).toHaveCount(0)

  expect(errors, "the page logged errors").toEqual([])
})

/**
 * A session picks what it may do, at the one moment it can — the same dialog
 * that asks where it sends. **Last in the file on purpose**: these tests share
 * one server, and starting a session ends the one the tests above are reading.
 * See
 * `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
 */
test("a session is started on a posture, and the page says which", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  // The server's own policy file, before anything is chosen. In Settings →
  // Models, which is where what this server resolved now lives.
  await page.click('.inspector .col-foot button[title="Settings"]')
  await page.click('.modal .rail button:has-text("Models")')
  await expect(page.locator("dt", { hasText: "Posture" }).first()).toBeVisible()
  await expect(page.locator(".settings dd").filter({ hasText: "this server's own" }))
    .toBeVisible()
  await page.locator(".modal-head button.link", { hasText: "close" }).click()
  await expect(page.locator("dialog.modal")).toHaveCount(0)

  // The "+" in the chat's head. Sessions were a tab strip over the chat
  // column (`RECORD/2026-09-15.a-three-pane-inspector.completed.md`) and before that
  // a `+ New` button beside a dropdown in the page header; one session is on
  // screen, so the head names it and the strip became the history popover.
  await page.click('.chat .acts button[title*="New session"]')
  const starter = page.getByRole("dialog", { name: "Where a session sends" })
  await expect(starter).toBeVisible()

  // Offered by name, out of config.toml. The page cannot add to the list: a
  // posture is a policy file, and one the browser could write is a sandbox the
  // browser could widen.
  const picker = starter.locator("select").last()
  await expect(picker.locator("option")).toHaveText([
    "this server's own policy file",
    "wide",
  ])
  await picker.selectOption("wide")
  await starter.locator('button:has-text("Start session")').click()
  await expect(starter).toBeHidden({ timeout: 30_000 })

  // And it is in force: Models opens on what this server resolved, and it
  // resolved the file the posture named. Reached from the composer's second
  // row, which is where the destination is reported now — the two tags that
  // used to be in the page header.
  await page.click(".options .dest")
  await page.locator('.options .menu button:has-text("providers…")').click()
  await expect(page.locator("dialog.modal").first()).toBeVisible()
  await expect(page.locator(".modal .rail button.on")).toHaveText("Models")
  const said = page.locator(".settings")
  await expect(said).toContainText("wide")
  await expect(said).toContainText("network allowed")
  await expect(page.locator("pre.sandbox")).toContainText("ls")
})

/**
 * The shell itself: the three columns, the switch that chooses what the left
 * one shows, and the chat's head. The layout carries the panels every later
 * phase of
 * `RECORD/2026-09-15.a-three-pane-inspector.completed.md` adds, so a column that
 * silently stopped rendering is worth catching here rather than in the
 * phase that builds against it.
 */
test("the three columns are there, each with a head and a foot", async ({ page }) => {
  // Wide enough for all three: below 1260 the second column switches between
  // content and chat instead, which the test after this one is about.
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  await expect(page.locator(".app")).toHaveAttribute("data-columns", "3")
  await expect(page.locator(".col.inspector")).toBeVisible()
  await expect(page.locator(".col.content")).toBeVisible()
  await expect(page.locator(".col.chat")).toBeVisible()
  // The skeleton every column now shares, and the reason the page-wide header
  // came out: three heads and three feet, all the same height.
  await expect(page.locator(".col-head")).toHaveCount(3)
  await expect(page.locator(".col-foot")).toHaveCount(3)
  const heights = await page.locator(".col-head, .col-foot").evaluateAll(
    nodes => nodes.map(node => Math.round(node.getBoundingClientRect().height)),
  )
  // Five at 40 and one at 80: the chat's foot is two rows, and that is the
  // only exception.
  expect(heights.filter(h => h === 40)).toHaveLength(5)
  expect(heights.filter(h => h === 80)).toHaveLength(1)
  // The page-wide `<header>` is gone, and staying gone is the point.
  await expect(page.locator("#app > header, .app > header")).toHaveCount(0)

  // Debug is the default panel, and it is the one that used to be the whole
  // right column: its turn picker is the cheapest proof it actually mounted.
  await expect(page.locator(".inspector .tabs button.on")).toHaveText("Debug")
  await expect(page.locator(".inspector .turn-picker")).toBeVisible()

  // The other two mount and replace it. What each one *shows* is asserted by
  // its own test below; this is about the switch.
  await page.click('.inspector .tabs button:has-text("Files")')
  await expect(page.locator(".inspector .tree")).toBeVisible({ timeout: 15_000 })
  await expect(page.locator(".inspector .turn-picker")).toBeHidden()

  await page.click('.inspector .tabs button:has-text("Git")')
  await expect(page.locator(".inspector h2")).toHaveText("Git")
  await expect(page.locator(".inspector .tree")).toBeHidden()

  await page.click('.inspector .tabs button:has-text("Debug")')
  await expect(page.locator(".inspector .turn-picker")).toBeVisible()

  // The chat's head: the session's name, and the three controls beside it.
  // One session is on screen, so the strip of tabs became a name in words and
  // a history popover — see
  // `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
  await expect(page.locator(".chat .name")).not.toBeEmpty()
  // Two, not three: the cog moved to the inspector's foot, which is the column
  // that is always on screen. See
  // `RECORD/2026-09-16.the-modals-are-dialogs.completed.md`.
  await expect(page.locator(".chat .acts button")).toHaveCount(2)
  await expect(page.locator('.inspector .col-foot button[title="Settings"]')).toHaveCount(1)
  // Drawn rather than typed since phase 7 of the three-pane record — this used
  // to assert the text "+". Every symbol on the page is a `<use>` of the
  // sprite in `app.html`, so a sprite that stopped rendering blanks all of
  // them at once and is worth one assertion of its own.
  await expect(page.locator("svg.sprite symbol")).toHaveCount(16)
  await expect(page.locator('.chat .acts use[href="#i-plus"]')).toHaveCount(1)

  // The history is the old strip. The live session cannot be deleted — the
  // server refuses it — so the row that is on carries no ×.
  await page.click('.chat .acts button[title="Earlier sessions"]')
  await expect(page.locator(".chat .history .row").first()).toBeVisible()
  await expect(page.locator(".chat .history .row.on")).toHaveCount(1)
  await expect(page.locator(".chat .history .row.on .close")).toHaveCount(0)
})

/**
 * Below 1260px there are two columns and the second one is a choice rather
 * than a casualty. The layout this replaced dropped the content column at
 * 64rem and gave no way to get it back, which answered *which two columns*
 * with the viewport instead of with the person. See
 * `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
 */
test("under 1260px the second column switches between content and chat", async ({ page }) => {
  await page.setViewportSize({ width: 1100, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  await expect(page.locator(".app")).toHaveAttribute("data-columns", "2")
  await expect(page.locator(".col.inspector")).toBeVisible()
  // Exactly one of the two, never both and never neither.
  const second = page.locator(".col.content, .col.chat")
  await expect(second).toHaveCount(1)

  // Whichever it is, its head carries the way to the other one.
  await page.click(".col-head .swap")
  await expect(second).toHaveCount(1)
  await page.click(".col-head .swap")
  await expect(second).toHaveCount(1)

  // Opening something in the content column switches to it: the click already
  // said which column the person wants, and having nothing happen is the bug
  // this rule exists to prevent.
  await page.click(".logo")
  await expect(page.locator(".col.chat")).toBeVisible()
  await page.click('.inspector .tabs button:has-text("Files")')
  await expect(page.locator(".inspector .tree .row").first()).toBeVisible({ timeout: 15_000 })
  await page.click('.inspector .tree .row:has-text("luu.toml")')
  await expect(page.locator(".col.content")).toBeVisible()
  await expect(page.locator(".col.chat")).toHaveCount(0)

  // And the logo is the way back, which is what it means in both layouts.
  await page.click(".logo")
  await expect(page.locator(".col.chat")).toBeVisible()

  // Three columns again the moment there is room.
  await page.setViewportSize({ width: 1440, height: 900 })
  await expect(page.locator(".app")).toHaveAttribute("data-columns", "3")
})

/**
 * The tree's row: three controls in one highlight, and an edit icon that is
 * only there under the pointer.
 *
 * The row stopped being a `<button>` to make room for it — jq79 builds
 * templates with `createElement`, which nests a button inside a button
 * happily, and then one click fires both handlers. See
 * `RECORD/2026-09-16.the-tree-makes-room-for-an-icon.completed.md`.
 */
test("the tree shows an edit icon on hover, and the name does not move", async ({ page }) => {
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)
  await page.click('.inspector .tabs button:has-text("Files")')
  const node = page.locator('.inspector .tree .node:has-text("luu.toml")').first()
  await expect(node).toBeVisible({ timeout: 15_000 })

  // No nesting: the two buttons are siblings inside the row, not one in the
  // other, which is what stops a click on the icon also opening the file.
  await expect(node.locator("button.row button")).toHaveCount(0)
  await expect(node.locator("> button")).toHaveCount(2)

  const edit = node.locator("button.edit")
  const name = node.locator(".name")

  // Present but invisible, and — the part that matters — not clickable while
  // invisible.
  await expect(edit).toHaveCSS("opacity", "0")
  await expect(edit).toHaveCSS("pointer-events", "none")

  // Measured until there is something to measure: the row can be rebuilt
  // under the test, and a locator read in that frame has no box — which is
  // what made this fail one run in three, before and after the engines work.
  const boxOf = async locator => {
    for (let tries = 0; tries < 50; tries++) {
      const box = await locator.boundingBox()
      if (box) return box
      await page.waitForTimeout(50)
    }
    throw new Error("the element never had a box")
  }

  // The slot is reserved, so revealing the icon must not move the name.
  const before = await boxOf(name)
  await node.hover()
  await expect(edit).toHaveCSS("opacity", "1")
  const after = await boxOf(name)
  expect(Math.round(after.width)).toBe(Math.round(before.width))

  // And the icon is the rightmost thing in the row.
  const nameBox = await boxOf(name)
  const editBox = await boxOf(edit)
  expect(editBox.x).toBeGreaterThan(nameBox.x + nameBox.width - 1)

  // A keyboard can find it: hover is not the only way it appears.
  await page.locator("button.row", { hasText: "luu.toml" }).first().focus()
  await page.keyboard.press("Tab")
  await expect(edit).toBeFocused()
  await expect(edit).toHaveCSS("opacity", "1")
})

/**
 * ESC, which means four different things depending on what is open, and the
 * settings button that moved out of the chat's head to say so.
 *
 * The modals are `<dialog>` elements opened with `showModal()`, so ESC is the
 * platform's rather than a key handler of this page's — what is asserted here
 * is that the page did not *also* act on the same keypress, and that the one
 * dialog which must not be dismissed still is not. See
 * `RECORD/2026-09-16.the-modals-are-dialogs.completed.md`.
 */
test("ESC closes what is open, and swaps the columns when nothing is", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  await page.setViewportSize({ width: 1100, height: 900 })
  await page.goto(`${BASE}/index.html`)

  // A first visit opens the folder picker and it cannot be dismissed: there is
  // nothing behind it to go back to. ESC has to refuse with it, or the browser
  // would close a dialog the page still believes is open.
  const picker = page.locator("dialog.modal")
  await expect(picker).toBeVisible()
  await page.keyboard.press("Escape")
  await expect(picker).toBeVisible()
  await picker.locator('button:has-text("Use this folder")').click()
  await expect(picker).toBeHidden()

  // Settings is in the inspector's foot now — the column that is always on
  // screen — rather than in the chat's head, which is a head that disappears
  // whenever two columns are showing the content.
  await expect(page.locator('.chat .acts button[title="Settings"]')).toHaveCount(0)

  await expect(page.locator(".app")).toHaveAttribute("data-columns", "2")
  const shown = async () =>
    (await page.locator(".col.content").count()) ? "content" : "chat"
  const before = await shown()

  // ESC over a modal closes the modal and nothing else: the column underneath
  // it must be the one it was.
  await page.click('.inspector .col-foot button[title="Settings"]')
  await expect(page.locator("dialog.modal")).toBeVisible()
  await page.keyboard.press("Escape")
  await expect(page.locator("dialog.modal")).toHaveCount(0)
  expect(await shown()).toBe(before)

  // With nothing left to cancel, it swaps them — and back.
  await page.keyboard.press("Escape")
  expect(await shown()).not.toBe(before)
  await page.keyboard.press("Escape")
  expect(await shown()).toBe(before)

  // Three columns are all on screen, so there is nothing to swap and ESC is
  // inert rather than doing something arbitrary.
  await page.setViewportSize({ width: 1440, height: 900 })
  await expect(page.locator(".app")).toHaveAttribute("data-columns", "3")
  await expect(page.locator(".col.content")).toBeVisible()
  await expect(page.locator(".col.chat")).toBeVisible()
  await page.keyboard.press("Escape")
  await expect(page.locator(".col.content")).toBeVisible()
  await expect(page.locator(".col.chat")).toBeVisible()

  expect(errors).toEqual([])
})

/**
 * The workspace panels, against this repository itself: the tree the server
 * walks is the checkout the test runs from, so `.gitignore` and `git status`
 * have real answers to give. Phases 3 and 4 of
 * `RECORD/2026-09-15.a-three-pane-inspector.completed.md`.
 *
 * Deliberately not asserting *which* files are changed — that depends on who
 * is running it and when. What is asserted is the shape: a tree that lists,
 * ignored entries marked as ignored, a file that opens with its lines, and a
 * diff that arrives as hunks rather than as text.
 */
test("the files panel lists the workspace, and a file opens in the viewer", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  await page.click('.inspector .tabs button:has-text("Files")')
  const rows = page.locator(".inspector .tree .row")
  await expect(rows.first()).toBeVisible({ timeout: 15_000 })
  // `luu.toml` is in every checkout; `target/` is ignored in every checkout
  // that has been built, and this suite needs a built binary to run at all.
  await expect(rows.filter({ hasText: "luu.toml" })).toHaveCount(1)
  await expect(page.locator(".inspector .tree .node.ignored").first()).toBeVisible()

  await page.click('.inspector .tree .row:has-text("luu.toml")')
  await expect(page.locator(".content .col-foot .path")).toHaveText("luu.toml")
  // The real file's first line, so this fails if the viewer renders someone
  // else's bytes under that name.
  await expect(page.locator(".content .code-rows li").first())
    .toContainText("luu's own sandbox")

  // A directory opens in place rather than replacing the tree.
  const before = await rows.count()
  await page.click('.inspector .tree .row:has-text("crates")')
  await expect(rows).not.toHaveCount(before)

  expect(errors, "the page logged errors").toEqual([])
})

test("the git panel lists changes, and one opens as a diff of hunks", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  await page.click('.inspector .tabs button:has-text("Git")')
  const changes = page.locator(".inspector .changes .row")
  // A clean checkout is a legitimate state, and then there is nothing to
  // click — the panel says so and this test has made its point either way.
  const empty = page.locator(".inspector .col-main p", { hasText: "Nothing changed" })
  await expect(changes.first().or(empty)).toBeVisible({ timeout: 15_000 })

  if (await changes.count()) {
    // Git's own two-position code, untrimmed: the space is which side changed,
    // and trimming it made every unstaged file look staged.
    await expect(page.locator(".inspector .changes .code").first()).toHaveText(/^[ MADRCU?!]{2}$/)
    await changes.first().click()
    await expect(page.locator(".content .col-foot .path")).not.toBeEmpty()
    // Either hunks, or the note that says why there are none (an untracked
    // file has no diff). An empty panel with neither is the failure.
    const hunks = page.locator(".content .hunk").first()
    const note = page.locator(".content .diff .pad")
    await expect(hunks.or(note)).toBeVisible({ timeout: 15_000 })
    // Both sides are reachable, which is the whole reason the toggle is there.
    await expect(page.locator('.content .col-foot .which button:has-text("staged")')).toBeVisible()
  }

  expect(errors, "the page logged errors").toEqual([])
})

/**
 * Highlighting, and the icon fallback. The gate server names no icon theme,
 * so this is the unconfigured half of phase 5 — that the tree draws its own
 * glyphs rather than nothing — and all of phase 6, which needs no
 * configuration at all. See
 * `RECORD/2026-09-15.a-three-pane-inspector.completed.md`.
 */
test("a source file arrives highlighted, and an unthemed tree still has glyphs", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  await page.click('.inspector .tabs button:has-text("Files")')
  await expect(page.locator(".inspector .tree .row").first()).toBeVisible({ timeout: 15_000 })
  // No `[ui] icon-theme` in this server's config, so: no theme art, and the
  // page's own shape on every row instead of a column of nothing. Which of
  // the two it is depends on the row, so both are named — the condition is the
  // row's own icon, and a bug in it shows up as a row with neither.
  await expect(page.locator(".inspector .tree img.icon")).toHaveCount(0)
  await expect(page.locator(".inspector .tree svg.glyph").first()).toBeVisible()
  await expect(page.locator('.inspector .tree use[href="#i-folder"]').first()).toBeVisible()
  await expect(page.locator('.inspector .tree use[href="#i-file"]').first()).toBeVisible()
  // And a directory row carries the caret that opens it.
  await expect(page.locator('.inspector .tree .twist use[href="#i-caret"]').first()).toBeVisible()

  // And the panel says *why* the tree looks like that, which is item 15 of
  // `ROADMAP/2026-09-17` and the reason it was a row at all: the first person
  // to meet an unconfigured tree read a working fallback as a broken panel, and
  // nothing on the page named the key that replaces it. See
  // `RECORD/2026-09-17.the-gate-panel-narrows.completed.md`.
  await page.click('.inspector .col-foot button[title="Settings"]')
  const icons = page.locator(".settings dt", { hasText: "Icons" }).locator("xpath=..")
  await expect(icons).toContainText("the tree's own two shapes")
  await expect(icons).toContainText("[ui] icon-theme")
  await page.locator(".modal-head button.link", { hasText: "close" }).click()
  await expect(page.locator("dialog.modal")).toHaveCount(0)

  // A Rust file, opened through the store the way a click does, because this
  // one is several directories down and the point is the highlighting.
  await page.evaluate(async () => {
    const { showFile } = await import("./lib/workspace.js")
    await showFile("crates/luu/src/highlight.rs")
  })
  await expect(page.locator(".content .col-foot .lang")).toHaveText("rust")
  // Longer than the viewer's first block, which is what makes this worth
  // asserting: the page renders a screenful and fills in the rest one frame
  // later, so a tail that never arrives leaves a file that looks whole and is
  // not. See the phase 8 sections of
  // `RECORD/2026-09-15.a-three-pane-inspector.completed.md`.
  const rows = await page.evaluate(async () =>
    (await import("./lib/workspace.js")).workspace.content.lines.length)
  expect(rows, "this file is meant to outrun the first block").toBeGreaterThan(200)
  await expect(page.locator(".content .code-rows li")).toHaveCount(rows)
  // Three captures that any Rust file has, so this fails if the grammar stops
  // loading or the chunks stop carrying their kind.
  await expect(page.locator(".content code.hl-keyword").first()).toBeVisible()
  await expect(page.locator(".content code.hl-comment").first()).toBeVisible()
  await expect(page.locator(".content code.hl-string").first()).toBeVisible()

  // Found by its name, having no extension to be found by.
  await page.evaluate(async () => {
    const { showFile } = await import("./lib/workspace.js")
    await showFile("Makefile")
  })
  await expect(page.locator(".content .col-foot .lang")).toHaveText("make")
  await expect(page.locator(".content code.hl-function").first()).toBeVisible()

  // A file with no grammar takes the same path out: lines, no language.
  await page.evaluate(async () => {
    const { showFile } = await import("./lib/workspace.js")
    await showFile(".gitignore")
  })
  await expect(page.locator(".content .col-foot .path")).toHaveText(".gitignore")
  await expect(page.locator(".content .col-foot .lang")).toHaveCount(0)
  await expect(page.locator(".content .code-rows li").first()).toBeVisible()

  expect(errors, "the page logged errors").toEqual([])
})

/**
 * The content column holds more than one thing at a time. Before this it held
 * exactly one and forgot it: `showFile` and `showDiff` overwrote the selection,
 * so opening a diff lost the file on screen with no way back that did not go
 * through the tree again. See
 * `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
 */
test("the content column keeps one tab per open thing", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  // Nothing open is a state the column says out loud rather than showing as a
  // blank strip.
  await expect(page.locator(".content .tabs.files .none")).toBeVisible()

  await page.click('.inspector .tabs button:has-text("Files")')
  await expect(page.locator(".inspector .tree .row").first()).toBeVisible({ timeout: 15_000 })
  // A single click is a preview, in italics, and the next single click
  // replaces it in place rather than adding a tab: VS Code's rule.
  const tabs = page.locator(".content .tabs.files .tab")
  await page.click('.inspector .tree .row:has-text("luu.toml")')
  await expect(tabs).toHaveCount(1)
  await expect(tabs.first()).toHaveClass(/preview/)
  await expect(tabs.first().locator(".label")).toHaveCSS("font-style", "italic")
  await page.click('.inspector .tree .row:has-text("Makefile")')
  await expect(tabs).toHaveCount(1)
  await expect(tabs.first().locator(".label")).toHaveText("Makefile")

  // A double click keeps it, and the next single click opens beside it.
  await page.dblclick('.inspector .tree .row:has-text("Makefile")')
  await expect(tabs.first()).not.toHaveClass(/preview/)
  await page.click('.inspector .tree .row:has-text("luu.toml")')
  await expect(tabs).toHaveCount(2)
  // So does a double click on the tab itself.
  await page.dblclick('.content .tabs.files .tab:has-text("luu.toml") .pick')
  await expect(page.locator(".content .tabs.files .tab.preview")).toHaveCount(0)
  await page.click('.content .tabs.files .tab:has-text("Makefile") .pick')

  // The one clicked is what the column is showing, and the foot says which.
  await expect(page.locator(".content .tabs.files .tab.on .label")).toHaveText("Makefile")
  await expect(page.locator(".content .col-foot .path")).toHaveText("Makefile")

  // And the first is still there to go back to, which is the whole point.
  await page.click('.content .tabs.files .tab:has-text("luu.toml") .pick')
  await expect(page.locator(".content .col-foot .path")).toHaveText("luu.toml")
  await expect(page.locator(".content .code-rows li").first()).toContainText("luu's own sandbox")

  // Opening the same file again focuses the tab rather than adding a second.
  await page.click('.inspector .tree .row:has-text("luu.toml")')
  await expect(page.locator(".content .tabs.files .tab")).toHaveCount(2)

  // Closing the active one lands on its neighbour rather than on nothing.
  await page.hover(".content .tabs.files .tab.on")
  await page.click(".content .tabs.files .tab.on .close")
  await expect(page.locator(".content .tabs.files .tab")).toHaveCount(1)
  await expect(page.locator(".content .col-foot .path")).toHaveText("Makefile")

  expect(errors, "the page logged errors").toEqual([])
})

/**
 * The folder the panels are rooted at, chosen under the one `luu serve` was
 * started in. Nothing here widens anything — every path still resolves through
 * `Sandbox::check_path` against that base, and the root is a prefix the page
 * narrows what it *lists* by. See
 * `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
 */
test("the workspace can be narrowed to a subdirectory of where serve started", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)

  // The forced picker: it covers the window and has no close, because on a
  // first visit there is nothing behind it to go back to.
  const picker = page.locator("dialog.modal").first()
  await expect(picker).toBeVisible({ timeout: 15_000 })
  await expect(picker.locator("button.link", { hasText: "close" })).toHaveCount(0)
  await expect(picker).toContainText(root)
  // Directories only: this chooses where a tree starts, and a file is not a
  // place a tree can start.
  await expect(picker.locator(".folders .row")).not.toHaveCount(0)
  await expect(picker.locator('.folders .row:has-text("Cargo.toml")')).toHaveCount(0)

  await picker.locator('.folders .row:has-text("crates")').click()
  await expect(picker.locator(".crumbs .crumb")).toHaveCount(2)
  await picker.locator('button:has-text("Use this folder")').click()
  await expect(picker).toBeHidden()

  // The tree starts there, and the foot says so.
  await expect(page.locator(".inspector .col-foot .root")).toContainText("crates")
  await page.click('.inspector .tabs button:has-text("Files")')
  const rows = page.locator(".inspector .tree .row")
  await expect(rows.first()).toBeVisible({ timeout: 15_000 })
  await expect(rows.filter({ hasText: "agent-core" })).toHaveCount(1)
  // And what is above it is not listed: `luu.toml` is in the base, not in
  // `crates/`.
  await expect(rows.filter({ hasText: "luu.toml" })).toHaveCount(0)

  // Remembered, so the question is asked once per server rather than per
  // visit.
  await page.reload()
  await expect(page.locator("dialog.modal")).toHaveCount(0)
  await expect(page.locator(".inspector .col-foot .root")).toContainText("crates")

  expect(errors, "the page logged errors").toEqual([])
})

/**
 * The optional editor, in whichever state this checkout is in.
 *
 * Monaco is an npm dependency of `web/`, gitignored and excluded from
 * the embedded UI, so *not installed* is the ordinary state for a checkout that
 * ran `cargo build` and nothing else — and it is the state CI is in. Both halves
 * are asserted because both are real: the setting says why it is unavailable, or
 * it works. Neither is a skip, because a skipped test is a test that is not
 * there. See
 * `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
 */
test("the editor setting offers Monaco where it is installed and says so where it is not", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  // The server answers the question rather than the page reading a 404 off a
  // missing file — which works, and logs a console error on every visit of
  // every checkout without it.
  const installed = await page.evaluate(async () =>
    (await (await fetch("./api/monaco")).json()).installed)

  await page.click('.inspector .col-foot button[title="Settings"]')
  const monaco = page.locator('.modal .seg button:has-text("Monaco")')
  await expect(monaco).toBeVisible()

  if (!installed) {
    await expect(monaco).toBeDisabled()
    await expect(page.locator(".modal .caveat", { hasText: "Monaco is not installed" }))
      .toBeVisible()
    expect(errors, "the page logged errors").toEqual([])
    return
  }

  await monaco.click()
  await page.locator(".modal-head button.link", { hasText: "close" }).click()
  await expect(page.locator("dialog.modal")).toHaveCount(0)

  await page.click('.inspector .tabs button:has-text("Files")')
  await expect(page.locator(".inspector .tree .row").first()).toBeVisible({ timeout: 15_000 })
  await page.click('.inspector .tree .row:has-text("Makefile")')

  // It draws instead of this page's viewer, not beside it.
  await expect(page.locator("#monaco-host .view-line").first()).toBeVisible({ timeout: 30_000 })
  await expect(page.locator(".content .code-rows li")).toHaveCount(0)

  // And switching back disposes it: an editor left attached to a detached
  // element is a leak that only shows up after twenty tab switches.
  await page.click('.inspector .col-foot button[title="Settings"]')
  await page.click('.modal .seg button:has-text("This page")')
  await page.locator(".modal-head button.link", { hasText: "close" }).click()
  await expect(page.locator("#monaco-host")).toHaveCount(0)
  await expect(page.locator(".content .code-rows li").first()).toBeVisible()

  expect(errors, "the page logged errors").toEqual([])
})

/**
 * Starts a session on the wide posture and leaves a gate open on it. The two
 * tests below each want their own, because approving one consumes it.
 *
 * **Its plan is empty, and that is not a shortcut.** A session started from the
 * page builds its own backend out of `config.toml` (`crate::backend_for`), so
 * it never sees the queue `--mock-reply` filled: the call gets the default
 * mock's paragraph, no `plan` block parses, and the proposal falls back to the
 * draft's own objective declaring nothing — which is the ordinary case for a
 * 7B and exactly the state the gate exists for. Everything the job may do is
 * therefore typed at the gate, which is what these tests are about.
 *
 * It therefore goes through the **explicit** door. Prose is not a suggestion,
 * so a model that answers in it opens no gate by itself, and *plan this* is how
 * a person asks for one over the draft.
 */
async function gateOnWidePosture(page) {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  await page.click('.chat .acts button[title*="New session"]')
  const starter = page.getByRole("dialog", { name: "Where a session sends" })
  await expect(starter).toBeVisible()
  await starter.locator("select").last().selectOption("wide")
  await starter.locator('button:has-text("Start session")').click()
  await expect(starter).toBeHidden({ timeout: 30_000 })

  const composer = page.locator(".composer input")
  await expect(composer).toBeEnabled()
  await composer.fill("what is published?")
  await page.click('.composer button[type="submit"]')

  // The drafting turn lands, which is what opens the draft to plan over.
  const live = page.locator(".live-job")
  await expect(live).toBeVisible({ timeout: 30_000 })
  await live.locator("button", { hasText: "plan this" }).click()

  const gate = page.locator(".gate")
  await expect(gate).toBeVisible({ timeout: 30_000 })
  return gate
}

/** The plan the server is actually holding, as approved. */
async function livePlan(page) {
  return await page.evaluate(async () => {
    const live = await (await fetch("./api/sessions/live")).json()
    return live.jobs[live.jobs.length - 1].plan
  })
}

/**
 * What a job may reach, shown and then narrowed, from the only surface that
 * approves anything.
 *
 * `network`, `egress` and `enforcement` have been on the protocol since
 * 2026-09-06 and were reachable from a script and from a client — and from
 * nothing a person uses. The panel did not even display them, so a plan asking
 * for the network was signed off by somebody who was never told it had asked.
 *
 * Driven through the browser rather than the socket on purpose: `serve_ws.rs`
 * already sends all three and passed throughout, because the protocol was never
 * the thing that was wrong. See
 * `RECORD/2026-09-17.the-gate-panel-narrows.completed.md`.
 */
test("the gate shows what a plan may reach, and a person narrows it there", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  const gate = await gateOnWidePosture(page)
  // The model declared nothing, so the whole of this job's sandbox is about to
  // be typed by a person — which is what the panel says in so many words.
  await expect(gate.locator("p", { hasText: "declared nothing" })).toBeVisible()

  // Two domains and the network they imply, added by hand. `Plan::amend` merges
  // lists into the plan's own rather than replacing them, and a domain grants
  // the network to reach it.
  const box = '.amend input[placeholder*="the plan forgot"]'
  await page.fill(box, "crates.io")
  await page.click('.amend button:has-text("add domain")')
  await page.fill(box, "*.github.com")
  await page.click('.amend button:has-text("add domain")')
  await expect(gate.locator("p", { hasText: "adding egress:" })).toContainText("*.github.com")

  const network = gate.locator(".narrow", { hasText: "network" })
  await network.locator('button:has-text("grant")').click()
  await expect(network.locator("button.on")).toHaveText("grant")

  // And the kernel asked to hold this job's children, whatever the session
  // settled on. A tightening: it needs no permission and cannot be refused.
  const enforcement = gate.locator(".narrow", { hasText: "enforcement" })
  await enforcement.locator('button:has-text("kernel")').click()
  await expect(enforcement.locator("button.on")).toHaveText("kernel")

  await page.click('.gate-buttons button:has-text("Approve")')
  await expect(gate).toBeHidden({ timeout: 15_000 })

  // The plan the job is held to, out of the read API — the assertion that says
  // the amendment left the browser rather than that a button changed colour.
  // The posture is `wide`, which grants the network and names no domains, so
  // all three survive `Plan::unmet`.
  const plan = await livePlan(page)
  expect(plan.network).toBe(true)
  expect(plan.egress).toEqual(["crates.io", "*.github.com"])
  expect(plan.enforcement).toBe("kernel")

  expect(errors, "the page logged errors").toEqual([])
})

/**
 * The other direction, which is the one that has to be refused.
 *
 * `enforcement` is the only field at this gate whose permissive value is the
 * one being asked for, so a person typing `best-effort` under a policy that
 * requires `kernel` is asking the policy file to relax. `Plan::unmet` says no,
 * the approval still goes through without it, and the refusal is where somebody
 * learns the floor is there at all — which is why the button is offered rather
 * than hidden.
 */
test("asking the gate to loosen enforcement is refused, and says by what", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  const gate = await gateOnWidePosture(page)
  const enforcement = gate.locator(".narrow", { hasText: "enforcement" })
  await enforcement.locator('button:has-text("best-effort")').click()
  // The panel says what is about to happen before it happens.
  await expect(gate.locator("p", { hasText: "refuses this" })).toBeVisible()

  await page.click('.gate-buttons button:has-text("Approve")')
  await expect(gate).toBeHidden({ timeout: 15_000 })

  const refused = page.locator(".refused")
  await expect(refused).toBeVisible()
  await expect(refused).toContainText("approve_plan")
  await expect(refused).toContainText("enforcement best-effort")

  // Refused at the gate means refused in the plan: the job runs under what the
  // policy file said, not under what the browser asked for.
  const plan = await livePlan(page)
  expect(plan.enforcement ?? null).toBe(null)

  expect(errors, "the page logged errors").toEqual([])
})
