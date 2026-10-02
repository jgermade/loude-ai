// @ts-check
/// The prose of an assistant reply, cut at its fenced code blocks, so a snippet
/// is drawn as code rather than as more of the sentence around it. Runs over
/// the text parts `segments.js` leaves — the tool calls are already out of it,
/// and a ```json block that was not a call is still here, and is a snippet.
///
/// A fence is a line that opens with three or more backticks (up to three
/// spaces in, as CommonMark has it), its info string's first word the
/// language; it closes on a line of at least as many backticks and nothing
/// else. One that never closes runs to the end of the text: while a reply
/// streams that is the block being written, and drawing it as code from its
/// first line beats flashing it as prose until the fence arrives.

/// `[{ kind: "text", text } | { kind: "code", lang, text, open }]`, in order.
/// `open` is a block whose closing fence has not come.
export function snippets(text) {
  // No fence, no change: the text part is returned as it came, newlines and all.
  if (!/^ {0,3}`{3,}/m.test(text)) return [{ kind: "text", text }]
  const out = []
  const lines = text.split("\n")
  let prose = []
  let code = null
  const flushProse = () => {
    // The newline that separated the prose from the fence belongs to neither.
    const joined = prose.join("\n").replace(/^\n+|\n+$/g, "")
    if (joined.trim()) out.push({ kind: "text", text: joined })
    prose = []
  }
  for (const line of lines) {
    if (!code) {
      const open = /^ {0,3}(`{3,})\s*([^`\s]*)[^`]*$/.exec(line)
      if (open) {
        flushProse()
        code = { fence: open[1].length, lang: open[2] || "", lines: [] }
      } else {
        prose.push(line)
      }
      continue
    }
    const close = /^ {0,3}(`{3,})\s*$/.exec(line)
    if (close && close[1].length >= code.fence) {
      out.push({ kind: "code", lang: code.lang, text: code.lines.join("\n"), open: false })
      code = null
    } else {
      code.lines.push(line)
    }
  }
  if (code) out.push({ kind: "code", lang: code.lang, text: code.lines.join("\n"), open: true })
  else flushProse()
  return out
}
