// @ts-check
import { expect, test } from "@playwright/test"
import { spawn } from "node:child_process"
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

/**
 * A write the floor refuses, put to the person in front of the page: the card
 * the call is drawn in shows what it would write and asks, and *Allow once*
 * writes it. Against a live `luu serve` on the mock, under this checkout's own
 * `luu.toml`, which grants `.` at read-write — so the floor is what refuses.
 *
 * The file lands in `.tmp/`, which is gitignored: this test's success *is* a
 * write into the checkout, and a red run must not be one into anything tracked.
 * See `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
 */

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..")
const PORT = 7901
const BASE = `http://127.0.0.1:${PORT}`
const TARGET = `.tmp/smoke-allow-once-${process.pid}.html`
const CONTENT = "<h1>allowed once</h1>"

const WRITE = `Creating it.
\`\`\`tool
{"name":"write_file","arguments":{"path":"${TARGET}","content":"${CONTENT}"}}
\`\`\``

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
  home = mkdtempSync(join(tmpdir(), "luu-ask-"))
  // A destination, or the composer stays disabled: the mock, named the way
  // `gate.spec.js` names it.
  writeFileSync(join(home, "config.toml"), '[provider.here]\nbackend = "mock"\nmodel = "mock"\n')
  mkdirSync(join(root, ".tmp"), { recursive: true })
  rmSync(join(root, TARGET), { force: true })
  server = spawn(
    binary(),
    [
      "serve",
      "--bind", `127.0.0.1:${PORT}`,
      "--no-store",
      "--mock-delay-ms", "0",
      // One write and its answer per test, in the order the tests run.
      "--mock-reply", WRITE,
      "--mock-reply", "Done.",
      "--mock-reply", WRITE,
      "--mock-reply", "Done.",
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
  rmSync(join(root, TARGET), { force: true })
})

/** The folder question a fresh browser gets first — see `gate.spec.js`. */
async function chooseFolder(page) {
  const picker = page.locator("dialog.modal").first()
  await picker.waitFor({ state: "visible", timeout: 5_000 }).catch(() => {})
  if (await picker.isVisible()) {
    await picker.locator('button:has-text("Use this folder")').click()
    await expect(picker).toBeHidden()
  }
}

test("a write the draft may not make is asked, and allowed once from the chat", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  const composer = page.locator(".composer input")
  await expect(composer).toBeEnabled()
  await composer.fill("crea un fichero example.html")
  await page.click('.composer button[type="submit"]')

  // The question, drawn open on the call it is about, with what would be
  // written and the floor's own words for why it was not.
  const card = page.locator(".call", { has: page.locator(".question") })
  await expect(card).toBeVisible({ timeout: 30_000 })
  await expect(card.locator(".name")).toHaveText("write_file")
  await expect(card.locator(".status")).toHaveText("waiting for you")
  await expect(card.locator(".question pre")).toHaveText(CONTENT)
  await expect(card.locator(".why")).toContainText("grants no writes")
  expect(existsSync(join(root, TARGET)), "nothing is written before the answer").toBe(false)

  await card.locator('button:has-text("Allow once")').click()

  const done = page.locator(".call", { hasText: "write_file" })
  await expect(done.locator(".status")).toHaveText("allowed once", { timeout: 15_000 })
  await expect(page.locator(".call .question")).toHaveCount(0)
  expect(readFileSync(join(root, TARGET), "utf8")).toBe(CONTENT)

  expect(errors).toEqual([])
})

test("a page that reloads while a call is held gets the question back", async ({ page }) => {
  const target = join(root, TARGET)
  rmSync(target, { force: true })
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)

  const composer = page.locator(".composer input")
  await expect(composer).toBeEnabled()
  await composer.fill("otra vez")
  await page.click('.composer button[type="submit"]')
  await expect(page.locator(".call .question")).toBeVisible({ timeout: 30_000 })

  // The turn is waiting on a person; the page that was showing the question
  // is gone. Without the question in the live view, nothing could answer it.
  await page.reload()
  await chooseFolder(page)
  const card = page.locator(".call", { has: page.locator(".question") })
  await expect(card).toBeVisible({ timeout: 15_000 })
  await expect(card.locator(".question pre")).toHaveText(CONTENT)

  await card.locator('button:has-text("Allow once")').click()
  await expect.poll(() => existsSync(target), { timeout: 15_000 }).toBe(true)
  expect(readFileSync(target, "utf8")).toBe(CONTENT)
})
