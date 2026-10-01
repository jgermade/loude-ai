// @ts-check
/// The prompt a turn was sent, cut into the budget's buckets so each piece can
/// wear its bucket's colour.
///
/// The server sends the buckets as *counts* and the prompt as *one string*;
/// nothing on the wire says where one bucket ends and the next begins. So the
/// cut is read back here from what the renderer writes the same way every
/// time — `Context::system_message`, `user_text`, a fold's pair:
///
/// - the system message is the system prompt, then `# Tools…`, then
///   `# Repository map…`, joined with blank lines, and an authority note
///   last when one is set in `system` position;
/// - a closed job is a user message (its objective) and an assistant message
///   opening `[job closed]` or `[draft closed]` — both **summaries**;
/// - every other exchange before the last user message is **history**;
/// - the last user message is this turn's fragments (`// path` blocks, the
///   **code**) and then the **prompt**, which ends with what was asked.
///
/// Where a boundary cannot be found, the text stays in the bucket it was
/// already in rather than being guessed into another: a colour is a claim.
/// See `RECORD/2026-09-30.the-foot-and-the-turn.completed.md`.

/// `blocks` is `[{ role, text }]`, as `turn-inspect.html` cuts the prompt at
/// its role markers. `asked` is what the person typed; `notes` the authority
/// notes' words this session was sent under. Returns the same blocks, each
/// with `parts: [{ bucket, text }]`.
export function paint(blocks, asked = "", notes = []) {
  const lastUser = blocks.map(b => b.role).lastIndexOf("user")
  return blocks.map((block, i) => {
    const next = blocks[i + 1]
    let parts
    if (block.role === "system") parts = system(block.text, notes)
    else if (isFold(block.text) || (block.role === "user" && next && isFold(next.text))) {
      parts = [{ bucket: "summaries", text: block.text }]
    } else if (i === lastUser) parts = question(block.text, asked, notes)
    else if (block.role === "prompt") parts = [{ bucket: "prompt", text: block.text }]
    else parts = [{ bucket: "history", text: block.text }]
    return { ...block, parts: parts.filter(p => p.text) }
  })
}

const isFold = text => /^\[(job|draft) closed\]/.test(text)

function system(text, notes) {
  const at = (header, from) => {
    const found = text.indexOf(`\n\n${header}`, from)
    return found < 0 ? -1 : found + 2
  }
  const tools = at("# Tools", 0)
  const map = at("# Repository map", Math.max(tools, 0))
  // The note renders last and counts with the system bucket, so it is
  // cut off the end before the map or the tools can claim it.
  const note = notes.find(n => n && text.endsWith(`\n\n${n}`))
  const end = note ? text.length - note.length - 2 : text.length
  const cuts = [
    { bucket: "system", from: 0 },
    ...(tools >= 0 ? [{ bucket: "tools", from: tools }] : []),
    ...(map >= 0 ? [{ bucket: "map", from: map }] : []),
  ]
  const parts = cuts.map((cut, k) => ({
    bucket: cut.bucket,
    text: text.slice(cut.from, Math.min(cuts[k + 1]?.from ?? end, end)),
  }))
  if (note) parts.push({ bucket: "system", text: text.slice(end) })
  return parts
}

/// This turn's user message: fragments, then the prompt. Fragments are only
/// looked for when the message opens with one (`// path`), and they end where
/// what was asked begins — found from the end, so a fragment quoting the
/// question does not move the cut. A note in `prompt` position renders just
/// before the question and counts as prompt, so it is moved back over when its
/// words are there.
function question(text, asked, notes) {
  const where = asked ? text.lastIndexOf(asked) : -1
  if (!text.startsWith("// ") || where <= 0) return [{ bucket: "prompt", text }]
  const before = text.slice(0, where)
  const note = notes.find(n => n && before.endsWith(`${n}\n\n`))
  const start = note ? where - note.length - 2 : where
  return [
    { bucket: "code", text: text.slice(0, start) },
    { bucket: "prompt", text: text.slice(start) },
  ]
}

/// The buckets in the order the prompt renders them, which is the legend's.
export const ORDER = ["system", "tools", "map", "summaries", "history", "code", "prompt"]

/// [`paint`]'s blocks, regrouped by bucket: `[{ bucket, entries: [{ role,
/// text }] }]`, one group per bucket that has text, in [`ORDER`]. What the
/// context *is made of* rather than the order it was sent in — the system
/// text, the tools, each earlier exchange under history, this turn's code and
/// its question — which is the question somebody opening it is asking. An
/// entry keeps the role of the message it came from, so history still reads
/// as who said what.
export function group(painted) {
  const groups = new Map()
  for (const block of painted) {
    for (const part of block.parts || []) {
      if (!part.text.trim()) continue
      const entries = groups.get(part.bucket) || []
      const last = entries[entries.length - 1]
      // Two pieces of one message in one bucket read as one entry.
      if (last && last.role === block.role && last.message === block) last.text += part.text
      else entries.push({ role: block.role, text: part.text, message: block })
      groups.set(part.bucket, entries)
    }
  }
  return [...ORDER, ...[...groups.keys()].filter(b => !ORDER.includes(b))]
    .filter(bucket => groups.has(bucket))
    .map(bucket => ({
      bucket,
      entries: groups.get(bucket).map(({ role, text }) => ({ role, text: text.trim() })),
    }))
}

/// The prompt as sent, cut at the role markers the renderer wrote
/// (`<|System|>`, `<|User|>`, `<|Assistant|>`). A prompt with no markers —
/// another renderer — is one block.
export function split(text) {
  if (!text) return []
  const found = [...text.matchAll(/<\|(\w+)\|>/g)]
  if (!found.length) return [{ role: "prompt", text }]
  return found.map((m, i) => ({
    role: m[1].toLowerCase(),
    text: text.slice(m.index + m[0].length, found[i + 1]?.index ?? text.length).trim(),
  }))
}
