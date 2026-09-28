// @ts-check
import { expect, test } from "@playwright/test"
import { spawn } from "node:child_process"
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

/**
 * Settings → Resend, driven from the browser against a live `luu serve`.
 *
 * The three rules that decide how much of the history a turn pays for again
 * were built, measured, tested and reachable only by typing a flag and
 * restarting the server. This is the surface that changes that, so this is the
 * test that opens it and clicks it — for the reason `gate.spec.js` exists at
 * all: two bugs in two days were found by a person opening this page and none
 * by a test. See
 * `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md` part 4.
 *
 * What it is really here for is the half a unit test cannot reach: **what the
 * page says after a save that did only part of what it was asked.** Turning
 * rule B off does not move a running session, and the person who clicked it has
 * to be told — by the server, reported by the page, and visible on screen.
 */

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..")
const PORT = 7896
const BASE = `http://127.0.0.1:${PORT}`

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
  // Its own state directory: this suite *writes* `config.toml`, so running it
  // against the config of the person running it would edit their machine.
  home = mkdtempSync(join(tmpdir(), "luu-settings-"))
  writeFileSync(
    join(home, "config.toml"),
    '[provider.here]\nbackend = "mock"\nmodel = "mock"\n',
  )
  server = spawn(
    binary(),
    [
      "serve",
      "--bind", `127.0.0.1:${PORT}`,
      "--mock-delay-ms", "0",
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

async function chooseFolder(page) {
  const picker = page.locator("dialog.modal").first()
  await picker.waitFor({ state: "visible", timeout: 5_000 }).catch(() => {})
  if (await picker.isVisible()) {
    await picker.locator('button:has-text("Use this folder")').click()
    await expect(picker).toBeHidden()
  }
}

test("the resend rules are chosen from the page, and a save says what it did not do", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await expect(page.locator(".col.inspector .logo")).toHaveText("luu")
  await chooseFolder(page)

  // The third section, which the modal's own comment said its shape made free.
  await page.click('.inspector .col-foot button[title="Settings"]')
  await page.click('.modal .rail button:has-text("Resend")')

  const running = page.locator(".modal .settings").first()
  await expect(running).toBeVisible()
  // What the server was started under with no flags: rule A on since
  // 2026-09-19 and the other two off, because A is the only one of the three
  // whose saving has been measured against a model.
  await expect(running.locator("dd").nth(0)).toContainText("once")
  await expect(running.locator("dd").nth(1)).toContainText("never")
  await expect(running.locator("dd").nth(2)).toContainText("kept")

  // The editor below it: this machine's default, which says nothing yet. Unset
  // is not off — it is the code's default, which for this rule is now `once`.
  const editor = page.locator(".modal .settings").nth(1)
  await expect(editor.locator(".seg").first().locator("button.on")).toHaveText("unset")

  // Rule A off, which since the flip is the direction that is a change. It
  // stores nothing, so it is the one rule that moves a running session in both
  // directions, and this is the half that used to be unreachable.
  await editor.locator('button:has-text("always")').click()
  await expect(page.locator(".modal button.save .count")).toHaveText("1")
  await page.locator(".modal button.save").click()
  // A save that landed says so on the button, until the next change.
  await expect(page.locator(".modal button.save.saved")).toBeVisible()

  await expect(running.locator("dd").nth(0)).toContainText("always")
  const off = await (await fetch(`${BASE}/api/resend`)).json()
  expect(off.file.repeat).toBe("always")
  expect(off.running.repeat).toBe("always")

  // And back on, which is the other direction and the one the default takes.
  await editor.locator('button:has-text("once")').click()
  await page.locator(".modal button.save").click()

  await expect(running.locator("dd").nth(0)).toContainText("once")
  const written = await (await fetch(`${BASE}/api/resend`)).json()
  expect(written.file.repeat).toBe("once")
  expect(written.running.repeat).toBe("once")
  // And in the file, beside the provider it must not have deleted.
  const onDisk = readFileSync(join(home, "config.toml"), "utf8")
  expect(onDisk).toContain("[resend]")
  expect(onDisk).toContain("[provider.here]")

  // Rule B on, which is clean: the prune line starts moving.
  await editor.locator('button:has-text("behind")').click()
  await page.locator(".modal button.save").click()
  await expect(running.locator("dd").nth(1)).toContainText("behind")

  // And rule B off again, which is the case this whole spec is for. The line is
  // a ratchet, so the running session keeps pruning; the file says never, and
  // the page has to say both rather than reporting a change that did not
  // happen.
  await editor.locator('button:has-text("never")').click()
  await page.locator(".modal button.save").click()

  const waiting = page.locator(".modal .waiting")
  await expect(waiting).toBeVisible()
  await expect(waiting).toContainText("ratchet")
  // The file moved and the session did not, and the panel above says so on the
  // row it is about.
  await expect(running.locator("dd").nth(1)).toContainText("behind")
  await expect(running.locator("dd").nth(1)).toContainText("the file says never")

  // Rule C is three values, not two, and the middle one is the point of part 5:
  // the 32 752 tokens it was justified by came entirely from `read_file` calls
  // and from no command output at all. Matched on an exact label, because
  // `has-text` is a substring and "cited" is one of "cited_reads".
  await editor.locator("button", { hasText: /^cited_reads$/ }).click()
  await page.locator(".modal button.save").click()

  await expect(running.locator("dd").nth(2)).toContainText("cited_reads")
  const cited = await (await fetch(`${BASE}/api/resend`)).json()
  expect(cited.file.results).toBe("cited_reads")
  expect(cited.running.results).toBe("cited_reads")

  // And stepping back down from it waits, for the same reason turning prune off
  // does: what it hands back has to fit somewhere, and what pays is the floor.
  await editor.locator("button", { hasText: /^kept$/ }).click()
  await page.locator(".modal button.save").click()
  await expect(page.locator(".modal .waiting")).toContainText("results:")
  await expect(running.locator("dd").nth(2)).toContainText("cited_reads")

  await page.locator(".modal-head button.link", { hasText: "close" }).click()
  await expect(page.locator("dialog.modal")).toHaveCount(0)

  expect(errors).toEqual([])
})

/**
 * Settings → Authority, [`the resend rules are chosen from the page`]'s reason
 * one section along: a person opening a page and clicking it has found bugs
 * a unit test did not. See
 * `RECORD/2026-09-22.an-authority-a-model-is-told.completed.md`.
 *
 * No `waiting` message to test here — that is the point of §"Applied live in
 * full" in `put_authority`'s own doc comment: a note has no ratchet a save
 * could be unsafe to move, so what is saved is what runs.
 */
test("an authority note is written from the page and reaches the live session", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await expect(page.locator(".col.inspector .logo")).toHaveText("luu")
  await chooseFolder(page)

  await page.click('.inspector .col-foot button[title="Settings"]')
  await page.click('.modal .rail button:has-text("Authority")')

  const running = page.locator(".modal .settings").first()
  await expect(running).toBeVisible()
  // Nothing set yet, on either authority.
  await expect(running.locator("dd").nth(0)).toContainText("nothing sent")
  await expect(running.locator("dd").nth(1)).toContainText("nothing sent")

  const editor = page.locator(".modal .settings").nth(1)
  await editor
    .locator("textarea")
    .first()
    .fill("You can read, but writes are refused until a plan is approved.")
  await page.locator(".modal button.save").click()

  await expect(running.locator("dd").nth(0)).toContainText(
    "You can read, but writes are refused",
  )
  // `system` is the default and is never sent as the empty string the
  // "unset" button writes for a rule — it renders as the word itself once a
  // note exists to have a position at all.
  await expect(running.locator("dd").nth(0)).toContainText("system")

  const written = await (await fetch(`${BASE}/api/authority`)).json()
  expect(written.file.draft.text).toContain("writes are refused")
  expect(written.running.draft.text).toContain("writes are refused")
  expect(written.running.draft.position).toBe("system")
  // The plan table is untouched.
  expect(written.file.plan).toBeUndefined()
  expect(written.running.plan).toBeNull()
  const onDisk = readFileSync(join(home, "config.toml"), "utf8")
  expect(onDisk).toContain("[authority.draft]")
  expect(onDisk).toContain("[provider.here]")

  // `prompt`, which repeats the note every turn instead of riding the cached
  // prefix once.
  await editor.locator('button:has-text("prompt")').first().click()
  await page.locator(".modal button.save").click()
  await expect(running.locator("dd").nth(0)).toContainText("prompt")
  const moved = await (await fetch(`${BASE}/api/authority`)).json()
  expect(moved.running.draft.position).toBe("prompt")

  await page.locator(".modal-head button.link", { hasText: "close" }).click()
  await expect(page.locator("dialog.modal")).toHaveCount(0)

  expect(errors).toEqual([])
})

/**
 * Settings → General → Files: an icon theme imported with the browser's folder
 * picker, written under `$LUU_HOME/icon-themes/`, chosen, and drawn by the tree
 * without a restart. See
 * `RECORD/2026-09-28.an-icon-theme-from-settings.completed.md`.
 */
test("an icon theme is imported from a picked folder and drawn at once", async ({ page }) => {
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  // A theme as small as one can be: an extension folder with one icon.
  const picked = join(mkdtempSync(join(tmpdir(), "luu-theme-")), "tiny-icons")
  mkdirSync(join(picked, "icons"), { recursive: true })
  writeFileSync(join(picked, "icons", "rust.svg"), '<svg xmlns="http://www.w3.org/2000/svg"/>')
  writeFileSync(
    join(picked, "theme.json"),
    JSON.stringify({
      iconDefinitions: { _rust: { iconPath: "./icons/rust.svg" } },
      fileExtensions: { rs: "_rust", toml: "_rust", md: "_rust" },
      file: "_rust",
    }),
  )
  writeFileSync(
    join(picked, "package.json"),
    JSON.stringify({
      name: "tiny-icons",
      contributes: { iconThemes: [{ id: "tiny", label: "Tiny Icons", path: "./theme.json" }] },
    }),
  )

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await chooseFolder(page)
  await page.click('.inspector button:has-text("Files")')
  await expect(page.locator(".inspector img")).toHaveCount(0)

  await page.click('.inspector .col-foot button[title="Settings"]')
  await page.locator(".modal input[type=file]").setInputFiles(picked)

  const row = page.locator(".modal .themes li", { hasText: "Tiny Icons" })
  await expect(row).toContainText("in use")
  const onDisk = readFileSync(join(home, "config.toml"), "utf8")
  expect(onDisk).toContain("icon-themes/tiny-icons")
  expect(onDisk).toContain("[provider.here]")

  await page.locator(".modal-head button.link", { hasText: "close" }).click()
  // Drawn by the tree at once, under the theme's new revision.
  await expect(page.locator(".inspector img").first()).toHaveAttribute("src", /\?r=1$/)

  rmSync(dirname(picked), { recursive: true, force: true })
  expect(errors).toEqual([])
})
