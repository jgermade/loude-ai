// An assistant reply as a person reads it: prose, and the tool calls between it.
//
// The model writes a call as text — a ```tool fence, or the bare object a 7B
// drops the fence down to — and the server streams that text as it comes,
// because the protocol carries what the model produced and not a reading of it.
// So the reading happens here, at render time, over the same two things the
// page has live and after a reload alike: the turn's text, which is every
// step's tokens concatenated in both cases, and its `tool_call`/`tool_result`
// pairs. Nothing is stored in this shape and nothing on the wire changed; the
// raw text is still what the inspector shows. See
// `RECORD/2026-09-29.a-switch-that-does-not-wait.completed.md`.
//
// Mirrors `agent_core::tools::parse_call`: the fenced form first, then a bare
// `{"name": …}` — here only at the start of a line, because this is a guess
// about what to *hide*, and prose that quotes an object mid-sentence should
// stay prose.

// Ours, and the one a model given its own trained tool prompt writes instead of
// its template's tags — `qwen2.5-coder:7b` on every call it made under the
// native transport. A ```json block is a call only when what is inside is one;
// any other JSON stays the code block it is. See
// `RECORD/2026-09-30.native-tool-calls.completed.md` and `agent_core::tools::CALL_FENCES`.
const FENCES = ["```tool", "```json"]

/// `[{ kind: "text", text }` | `{ kind: "tool", call, name, arguments, pending, raw }]`.
///
/// `call` is the executed call this block became, in order, or `null` where
/// none did: the turn ended on it (cancelled, out of steps) or the block did
/// not parse. `pending` is a block still being written — the placeholder.
/// `live` says the reply may still grow, which is when a tail that could be
/// the start of a call is held back instead of flashed.
export function segments(text, tools = [], live = false) {
  const calls = [...(tools || [])].sort((a, b) => a.step - b.step)
  const out = []
  let used = 0
  let i = 0
  const pushText = t => {
    if (!t) return
    const last = out[out.length - 1]
    if (last?.kind === "text") last.text += t
    else out.push({ kind: "text", text: t })
  }
  const pushTool = (parsed, raw, pending) => {
    const readable = parsed && typeof parsed.name === "string" && parsed.name
    const call = readable && !pending ? calls[used++] || null : null
    out.push({
      kind: "tool",
      call,
      name: call?.name || (readable ? parsed.name : ""),
      arguments: call?.arguments || parsed?.arguments || null,
      pending,
      raw,
    })
  }

  while (i < text.length) {
    const [fence, tag] = FENCES
      .map(f => [text.indexOf(f, i), f])
      .filter(([at]) => at >= 0)
      .sort((a, b) => a[0] - b[0])[0] || [-1, ""]
    const bare = bareStart(text, i)
    const next = [fence, bare].filter(n => n >= 0).sort((a, b) => a - b)[0]
    if (next === undefined) {
      pushText(live ? holdBack(text.slice(i)) : text.slice(i))
      break
    }
    pushText(text.slice(i, next))

    if (next === fence) {
      let body = next + tag.length
      if (text[body] === "\n") body += 1
      const close = text.indexOf("```", body)
      const json = tag === "```json"
      // A ```json block that is plainly not a call — anything but the start of
      // `{"name"` — is the model showing JSON, and stays text.
      if (json && !/^\s*(\{\s*("(n(a(m(e("\s*:?)?)?)?)?)?)?)?$/.test(text.slice(body, close < 0 ? undefined : close).slice(0, 16))
          && !/^\s*\{\s*"name"\s*:/.test(text.slice(body))) {
        const end = close < 0 ? text.length : close + 3
        pushText(text.slice(next, end))
        i = end
        continue
      }
      if (close < 0) {
        // Still being written, or never closed. Live it is the placeholder;
        // afterwards it is what the model left, shown as the block it is.
        if (live) pushTool(null, text.slice(next), true)
        else pushTool(parse(text.slice(body)), text.slice(next), false)
        break
      }
      const parsed = parse(text.slice(body, close))
      if (json && !parsed?.name) pushText(text.slice(next, close + 3))
      else pushTool(parsed, text.slice(next, close + 3), false)
      i = close + 3
      continue
    }

    const end = objectEnd(text, bare)
    if (end < 0) {
      if (live) pushTool(null, text.slice(bare), true)
      else pushText(text.slice(bare))
      break
    }
    const parsed = parse(text.slice(bare, end))
    if (parsed?.name) pushTool(parsed, text.slice(bare, end), false)
    else pushText(text.slice(bare, end))
    i = end
  }

  // Calls the text does not show in a form this file recognises — a bare
  // object mid-line, say. Still drawn: a call that ran is never hidden.
  while (used < calls.length) {
    const call = calls[used++]
    out.push({ kind: "tool", call, name: call.name, arguments: call.arguments, pending: false, raw: "" })
  }

  return out
    .map(part => part.kind === "text" ? { ...part, text: part.text.replace(/^\n+|\s+$/g, "") } : part)
    .filter(part => part.kind !== "text" || part.text)
}

function parse(body) {
  try {
    return JSON.parse(body.trim())
  } catch {
    return null
  }
}

/// The first `{` at the start of a line that opens `{"name"`, at or after `from`.
function bareStart(text, from) {
  const pattern = /(^|\n)[ \t]*(\{\s*"name"\s*:)/g
  pattern.lastIndex = from
  const match = pattern.exec(text)
  return match ? match.index + match[1].length + match[0].slice(match[1].length).indexOf("{") : -1
}

/// One past the `}` that closes the object opened at `start`, or -1 while it
/// is still open. Strings are skipped, escapes included, so a brace inside an
/// argument does not close the call early.
function objectEnd(text, start) {
  let depth = 0
  let quoted = false
  for (let i = start; i < text.length; i++) {
    const c = text[i]
    if (quoted) {
      if (c === "\\") i++
      else if (c === '"') quoted = false
    } else if (c === '"') quoted = true
    else if (c === "{") depth++
    else if (c === "}" && --depth === 0) return i + 1
  }
  return -1
}

/// A live tail that may be the first characters of a call, held back for the
/// frame or two until it is clearly one or clearly not — so the placeholder is
/// not preceded by a flash of backticks.
function holdBack(tail) {
  return tail
    .replace(/(^|\n)[ \t]*`{1,3}(t(o(ol?)?)?|j(s(on?)?)?)?$/, "$1")
    .replace(/(^|\n)[ \t]*\{\s*("(n(a(me?)?)?)?"?)?$/, "$1")
}
