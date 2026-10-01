// @ts-check
import { expect, test } from "@playwright/test"
import { execSync, spawn } from "node:child_process"
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

/**
 * The terminal panel, in a real container.
 *
 * Its own server on `luu.container.toml`, served from this checkout, and
 * **skipped where there is no Docker daemon or no `luu-worker:dev` image**:
 * building the image is minutes, and the page's half of the terminal is
 * nothing without the container's. `scripts/container-check.sh` builds the
 * image the same way. See
 * `RECORD/2026-10-01.a-terminal-in-the-container.completed.md`.
 */

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..")
const PORT = 7899
const BASE = `http://127.0.0.1:${PORT}`

const runs = command => {
  try {
    execSync(command, { stdio: "ignore" })
    return true
  } catch {
    return false
  }
}
const contained = runs("docker image inspect luu-worker:dev")

function binary() {
  const named = process.env.LUU_BIN
  const candidates = named
    ? [resolve(root, named)]
    : [join(root, "target/release/luu"), join(root, "target/debug/luu")]
  const found = candidates.find(path => existsSync(path))
  if (!found) throw new Error(`no luu binary at ${candidates.join(" or ")}`)
  return found
}

/** @type {import("node:child_process").ChildProcess | null} */
let server = null
let scratch = ""

test.beforeAll(async () => {
  if (!contained) return
  scratch = mkdtempSync(join(tmpdir(), "luu-terminal-"))
  // One posture besides the server's own, so the panel has somewhere to move
  // the session: this checkout's `luu.toml`, which runs on the host.
  mkdirSync(join(scratch, "home"), { recursive: true })
  writeFileSync(join(scratch, "home/config.toml"), '[posture.host]\npolicy = "luu.toml"\n')
  server = spawn(
    binary(),
    ["serve", "--bind", `127.0.0.1:${PORT}`, "--no-store", "--sandbox", "luu.container.toml"],
    // `SHELL`: see `workspace.spec.js` — the host shell, without anybody's rc.
    { cwd: root, env: { ...process.env, LUU_HOME: join(scratch, "home"), SHELL: "/bin/sh" }, stdio: "pipe" },
  )
  let output = ""
  server.stdout?.on("data", chunk => (output += chunk))
  server.stderr?.on("data", chunk => (output += chunk))
  const deadline = Date.now() + 60_000
  for (;;) {
    if (server.exitCode !== null) throw new Error(`luu serve exited with ${server.exitCode}:\n${output}`)
    try {
      if ((await fetch(`${BASE}/api/terminal`)).ok) return
    } catch {
      // Not listening yet.
    }
    if (Date.now() > deadline) throw new Error(`luu serve never answered:\n${output}`)
    await new Promise(again => setTimeout(again, 250))
  }
})

test.afterAll(() => {
  server?.kill("SIGTERM")
  if (scratch) rmSync(scratch, { recursive: true, force: true })
})

/** What is running in the session's container, by command name. */
function inside() {
  const name = execSync(`docker ps --format '{{.Names}}' --filter name=luu-worker-${server?.pid}-`)
    .toString().trim().split("\n")[0]
  return execSync(`docker exec ${name} ps -eo comm`).toString().split("\n").map(line => line.trim())
}

test("a terminal opens in the session's container, survives being hidden, and ends there", async ({ page }) => {
  test.skip(!contained, "no Docker daemon, or no luu-worker:dev image")
  const errors = []
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  page.on("console", message => {
    if (message.type() === "error") errors.push(`console: ${message.text()}`)
  })

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`${BASE}/index.html`)
  await page.locator('button:has-text("Use this folder")').click()

  const button = page.locator(".content .col-foot button.term")
  await expect(button).toHaveAttribute("title", "Open a terminal in the session's container")
  await button.click()
  const panel = page.locator(".content .terminal")
  // The terminal's controls are in the column's foot, under the panel.
  const foot = page.locator(".content .col-foot")
  const rows = panel.locator(".xterm-rows")
  await expect(foot.locator('.state.open[data-place="container"]')).toHaveCount(1, { timeout: 15_000 })
  // Named by where it runs, the posture under it, and the one posture that
  // runs elsewhere offered beside it.
  await expect(foot.locator(".dropup .face")).toHaveText("docker · luu-worker:dev")

  // The worker's uid, the base at its own path, and the colours the page draws.
  const uid = execSync("id -u").toString().trim()
  await page.keyboard.type("echo U=$(id -u) D=$(pwd) T=$TERM\n")
  await expect(rows).toContainText(`U=${uid} D=${root} T=xterm-256color`)

  // Hidden and shown: the same shell, not a new one.
  await page.keyboard.type("export MARK=kept; sleep 900 &\n")
  await button.click()
  await expect(panel).toHaveCount(0)
  await button.click()
  await panel.locator(".host").click()
  await page.keyboard.type("echo M=$MARK\n")
  await expect(rows).toContainText("M=kept")
  expect(inside()).toContain("sleep")

  // Moved to the host from the panel: the session moves, its container goes,
  // and the shell opens again on this machine. `/.dockerenv` is the one fact
  // that tells the two apart on any host, Linux included.
  const where = "test -f /.dockerenv && echo IN=con''tainer || echo IN=ho''st\n"
  await foot.locator(".dropup .face").click()
  await expect(foot.locator(".dropup .opt .hint", { hasText: "host · luu.toml" })).toHaveCount(1)
  await foot.locator(".dropup .opt", { has: page.locator(".label", { hasText: /^host$/ }) }).click()
  await expect(foot.locator('.state.open[data-place="host"]')).toHaveCount(1, { timeout: 15_000 })
  await expect(foot.locator(".dropup .face")).toHaveText("host")
  await panel.locator(".host").click()
  await page.keyboard.type(where)
  await expect(rows).toContainText("IN=host")
  // And back, into a container of its own — the first one went with the move.
  await foot.locator(".dropup .face").click()
  // Every container runtime is offered for the container posture, and the
  // ones this machine lacks are there, off, saying so.
  for (const runtime of ["podman", "nerdctl", "colima", "container"]) {
    const option = foot.locator(".dropup .opt", { has: page.locator(".label", { hasText: `${runtime} · luu-worker:dev` }) })
    if (!execSync(`command -v ${runtime} || true`, { shell: "/bin/sh" }).toString().trim()) {
      await expect(option).toBeDisabled()
      await expect(option.locator(".hint")).toContainText("not installed")
    }
  }
  await foot.locator(".dropup .opt", { has: page.locator(".label", { hasText: "docker · luu-worker:dev" }) }).click()
  await expect(foot.locator('.state.open[data-place="container"]')).toHaveCount(1, { timeout: 30_000 })
  await panel.locator(".host").click()
  await page.keyboard.type(where)
  await expect(rows).toContainText("IN=container")
  await page.keyboard.type("sleep 900 &\n")
  await expect.poll(() => inside()).toContain("sleep")

  // Ended from inside — there is no button for it — and nothing it started is
  // left in a container that outlives it.
  await page.keyboard.type("exit\n")
  await expect(foot.locator(".state.ended")).toHaveCount(1, { timeout: 15_000 })
  await button.click()
  await expect(panel).toHaveCount(0)
  await expect.poll(() => inside().filter(name => name === "bash" || name === "sleep"), { timeout: 10_000 })
    .toEqual([])

  expect(errors, "the page logged errors").toEqual([])
})
