// @ts-check
/// A reply's fenced blocks, coloured by the server's highlighter — the one that
/// colours files, so a snippet in the chat and the same code in the viewer wear
/// the same classes. See `crate::highlight::fenced`.
///
/// **Not a component.** A component in the transcript's `:each` is mounted
/// again whenever its row is handed a new part, and the parts are new on every
/// token: a snippet as a component was torn down and rebuilt seventeen times
/// while one reply streamed in, which is the flicker. The block is plain markup
/// in the row, which jq79 updates in place, and what it needs to remember — the
/// colours it has, and whether it was just copied — lives here, by slot.
///
/// **Coloured while it streams.** A block still being written is asked for at
/// most every `EVERY` ms; what came back stays painted, and the text that has
/// arrived since is added after it plain, so a token never takes the colours
/// away. A closed block is asked once more for its last lines. Never asked in a
/// replay, whose static host has no server to answer and would log the refusal.

import { $reactive } from "@web/vendor/jq79.js"
import { apiHeaders, state } from "./store.js"

/// `lit[slot]` is `{ text, lines }`: the text that was coloured, and its lines
/// as `/api/highlight` gives them. `copied[slot]` is set for a moment after a
/// copy. A slot is `<entry id>:<part index>`.
export const snippet = $reactive({ lit: {}, copied: {} })

const EVERY = 250
/// Per slot: the newest text wanted, and whether a request is out or waiting.
const asking = new Map()

/// Schedules the colours for a block, at most one request at a time per block.
/// Called from the template, so it only ever schedules: the request and the
/// write it ends in happen outside the effect that asked.
function want(slot, language, text, open) {
  if (!language || state.status === "replay") return
  if (snippet.lit[slot]?.text === text) return
  let slotState = asking.get(slot)
  if (!slotState) asking.set(slot, slotState = { busy: false, timer: null, latest: null })
  slotState.latest = { language, text }
  if (slotState.busy || slotState.timer) return
  slotState.timer = setTimeout(() => ask(slot), open ? EVERY : 0)
}

async function ask(slot) {
  const slotState = asking.get(slot)
  slotState.timer = null
  const { language, text } = slotState.latest
  slotState.busy = true
  let found = null
  try {
    const answer = await fetch("./api/highlight", {
      method: "POST",
      headers: { ...apiHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify({ language, text }),
    })
    found = answer.ok ? await answer.json() : null
  } catch {
    // No server, or it went away: the block stays plain.
  }
  slotState.busy = false
  if (found?.language) snippet.lit = { ...snippet.lit, [slot]: { text, lines: found.lines } }
  // More arrived while this one was out: once more, on the same beat.
  if (slotState.latest.text !== text && !slotState.timer) {
    slotState.timer = setTimeout(() => ask(slot), EVERY)
  }
}

const escape = text => text
  .replace(/&/g, "&amp;")
  .replace(/</g, "&lt;")
  .replace(/>/g, "&gt;")

/// The block as HTML for `:html`: the coloured lines it has, then whatever has
/// arrived since, plain. A `kind` is a capture name from `crate::highlight`,
/// never anything out of the reply; the reply's own text is escaped.
///
/// `colours` is `snippet.lit`, handed in by the template: jq79 redraws on what
/// an expression reads itself, not on what a function it calls reads, so a
/// binding that only called this would never see the colours arrive.
export function snippetHtml(slot, part, colours = snippet.lit) {
  want(slot, part.lang, part.text, part.open)
  const lit = colours[slot]
  if (!lit || !part.text.startsWith(lit.text)) return escape(part.text)
  const coloured = lit.lines
    .map(line => line
      .map(chunk => chunk.kind
        ? `<span class="hl-${chunk.kind.replace(/[^\w.-]/g, "")}">${escape(chunk.text)}</span>`
        : escape(chunk.text))
      .join(""))
    .join("\n")
  return coloured + escape(part.text.slice(lit.text.length))
}

/// Copies a block, and says so on its button for a moment. Where there is no
/// clipboard — an insecure origin, a refused permission — the text is still on
/// screen to select.
export async function copySnippet(slot, text) {
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    return
  }
  snippet.copied = { ...snippet.copied, [slot]: true }
  setTimeout(() => {
    const copied = { ...snippet.copied }
    delete copied[slot]
    snippet.copied = copied
  }, 1500)
}
