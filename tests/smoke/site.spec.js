// @ts-check
import { expect, test } from "@playwright/test"

/**
 * The one error the static twin is expected to produce: there is no server
 * behind it, so the socket the live page opens cannot connect, and the page
 * falls back to replaying a recording. Everything else is a real failure —
 * a component that did not mount, a fixture that does not parse, a store that
 * read a field the export does not write.
 */
const EXPECTED = /WebSocket|ws:\/\/|wss:\/\//i

/** Collects console errors and uncaught exceptions for the whole test. */
function watch(page) {
  const errors = []
  page.on("console", message => {
    if (message.type() === "error" && !EXPECTED.test(message.text())) {
      errors.push(`console: ${message.text()}`)
    }
  })
  page.on("pageerror", error => errors.push(`uncaught: ${error.message}`))
  return errors
}

test("the page mounts and replays a recording, with nothing in the console", async ({ page }) => {
  const errors = watch(page)

  await page.goto("/index.html")

  // Mounted at all: the components are fetched at runtime, so a broken path
  // here is an empty page rather than a build failure.
  await expect(page.locator(".col.inspector .logo")).toHaveAccessibleName("luu")

  // The static twin was found and the page fell back to it. The picker is the
  // signal: it renders only when `state.fixtures` was filled, which only the
  // static fallback does.
  //
  // Not the composer, and not the status word. Both say "live" while a recorded
  // turn is mid-replay — `replaying` is `status === "replay"`, and the status is
  // "running" until the turn ends, so the input is enabled and reads "Ask
  // something…" over a recording nobody can send into. Found by this test, left
  // alone by it: it is a real bug and fixing it is not what this change is.
  await expect(page.locator("select.fixtures")).toBeVisible()

  // The recording plays: a prompt and an answer, from the exported session.
  await expect(page.locator(".transcript article.user .text").first()).not.toBeEmpty()
  await expect(page.locator(".transcript article.assistant .text").first()).not.toBeEmpty({
    timeout: 20_000,
  })

  // The panel the whole debug client exists for. `history` is the bucket that
  // was empty in every recording made before eviction worked, so it is the one
  // worth naming here.
  await expect(page.locator(".inspector .legend li")).not.toHaveCount(0)
  await expect(page.locator(".inspector")).toContainText("tokens sent")

  expect(errors, `the page logged: ${errors.join(" | ")}`).toEqual([])
})

test("every recorded session in the picker can be selected", async ({ page }) => {
  const errors = watch(page)

  await page.goto("/index.html")
  const picker = page.locator("select.fixtures")
  await expect(picker).toBeVisible()

  const files = await picker.locator("option").evaluateAll(options =>
    options.map(option => option.value),
  )
  expect(files.length).toBeGreaterThan(1)

  // Each one is a real recording of the real protocol, so a fixture the store
  // cannot read is a format drift — the failure this exists to catch.
  for (const file of files) {
    await picker.selectOption(file)
    await expect(page.locator(".transcript article").first()).toBeVisible()
  }

  expect(errors, `the page logged: ${errors.join(" | ")}`).toEqual([])
})

test("a replayed eviction marks the turns that left the window", async ({ page }) => {
  // A recording replays at the pace it was recorded, and the first cut lands
  // halfway through this one — 11 turns in, because that is when the window
  // this fixture was recorded against actually filled up. Waiting for it is
  // the cost of a fixture that is a real run rather than a hand-made one.
  test.setTimeout(90_000)
  const errors = watch(page)

  await page.goto("/index.html")
  await expect(page.locator("select.fixtures")).toBeVisible()
  // The block policy cuts deep and rarely, so its first tombstone names six
  // turns at once — the policy's whole argument, in one message.
  await page.locator("select.fixtures").selectOption("./fixtures/eviction-block.jsonl")

  // The panel says which cut it was, which is the question a shrinking history
  // bucket could never answer on its own.
  await expect(page.locator(".inspector")).toContainText("out of the window, for good", {
    timeout: 60_000,
  })
  await expect(page.locator(".inspector")).toContainText("block")

  // And the turns are kept and marked, never removed: the transcript's whole
  // job is the difference between what happened and what the model still sees.
  await expect(page.locator(".transcript article.evicted").first()).toBeVisible()
  await expect(page.locator(".transcript article.evicted .gone").first())
    .toContainText("out of the window")

  expect(errors, `the page logged: ${errors.join(" | ")}`).toEqual([])
})

test("a replayed prune marks the turns whose code left the prompt", async ({ page }) => {
  // The counterpart of the test above, and the distinction the whole rule is
  // for: an evicted turn is gone from the conversation, a pruned one is still
  // asked, still answered and still citable — what left is its code. A page
  // that drew them the same way, or drew the second one not at all, is what
  // this catches.
  test.setTimeout(90_000)
  const errors = watch(page)

  await page.goto("/index.html")
  await expect(page.locator("select.fixtures")).toBeVisible()
  await page.locator("select.fixtures").selectOption("./fixtures/prune-behind.jsonl")

  // The panel names the turns that paid and what the window saved by it. The
  // `code` bucket shrinking is visible without this line and says neither.
  await expect(page.locator(".inspector")).toContainText("tokens of code out of the prompt", {
    timeout: 60_000,
  })
  await expect(page.locator(".inspector")).toContainText("still in the conversation")

  // And the transcript marks them without striking them through: the turn is
  // still there to be answered from.
  const cited = page.locator(".transcript article .cited").first()
  await expect(cited).toContainText("code out of the prompt since turn")
  await expect(page.locator(".transcript article.evicted")).toHaveCount(0)

  // And a cut belongs to the turn that made it. This arm prunes at turns 7, 8
  // and 10 and not at 9, which is the one turn in any fixture that can tell the
  // difference: the store used to carry the last cut forward into every turn it
  // kept afterwards, so turn 9 showed turn 8's prune — and a reload, which
  // builds each turn out of the read API instead, did not.
  const picker = page.locator(".turn-picker select")
  await expect(picker.locator("option[value='9']")).toHaveCount(1, { timeout: 60_000 })
  await picker.selectOption("9")
  await expect(page.locator(".inspector")).toContainText("Turn 9 as it was sent")
  await expect(page.locator(".inspector")).not.toContainText("tokens of code out of the prompt")

  expect(errors, `the page logged: ${errors.join(" | ")}`).toEqual([])
})

test("the repository map is a bucket of its own in the budget", async ({ page }) => {
  const errors = watch(page)

  await page.goto("/index.html")
  await expect(page.locator("select.fixtures")).toBeVisible()
  await page.locator("select.fixtures").selectOption("./fixtures/repo-map.jsonl")

  // The map is prefix, so it is in the budget like anything else in the
  // prompt — a block that were free would be a block that silently ate the
  // answer. The panel plots it beside the tools rather than inside them.
  const map = page.locator(".inspector .legend li").filter({ hasText: "map" })
  await expect(map).toHaveCount(1)
  const tokens = Number((await map.innerText()).replace(/[^0-9]/g, ""))
  expect(tokens).toBeGreaterThan(0)

  expect(errors, `the page logged: ${errors.join(" | ")}`).toEqual([])
})
