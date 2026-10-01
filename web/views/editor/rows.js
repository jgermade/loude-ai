// @ts-check
/// The file viewer's rows, built without the renderer.
///
/// **Why this is not a jq79 template.** The rows are the viewer's whole cost —
/// 271 ms of a 291 ms job, measured in
/// `RECORD/2026-09-16.what-the-debug-ui-does-not-need.completed.md` — and the
/// cost is per *chunk*, not per line: a `:each` inside a `:each` gives every
/// coloured span a scope object, a reactive scope, a `:class` effect and an
/// interpolation effect. A 3947-line file is ~21 000 standing subscriptions
/// over data that arrives whole, already coloured, and is never written to
/// again. Reactivity tracks what changes; these never change.
///
/// Measured rather than argued, and the record carries the table: the whole
/// file lands in **half** the time and the tab costs **a thirtieth** of the
/// heap. See `RECORD/2026-09-16.the-viewer-in-plain-js.completed.md`.
///
/// jq79 keeps the chrome around this — the placeholder, `loading`, `error`, the
/// tabs, the Monaco host — which is small and genuinely reactive. This module
/// owns only what is inside the `<ol>`.

/// Rows in the first pass: a screenful with room to scroll into, and nothing to
/// do with the window's real height — a viewer that measures itself has to
/// decide again on every resize. Unchanged from the `:each` version it
/// replaces.
export const FIRST_ROWS = 200

/// Appends one block of rows to the `<ol>`, leaving everything already in it
/// untouched.
///
/// **Appending rather than re-rendering is the point.** The two-block shape
/// this replaces existed because a reactive `:each` re-derives its list; the
/// phase 8 sections of `RECORD/2026-09-15.a-three-pane-inspector.completed.md` bought
/// a surviving first block by binding it to an array that never changes again.
/// Here nothing re-derives, so the first block survives for free — and the
/// number of blocks stops being a shape the template has to spell out.
///
/// **`createElement` rather than one `insertAdjacentHTML`, and the reason is
/// that it does not matter.** Both were built and timed against each other over
/// the same payload, interleaved so neither arm was always first: 208 ms
/// against 213 ms on a 3947-line Rust file, which is a tie. Building the whole
/// HTML string is 3 ms of that ~210 — 99% is constructing nodes and laying them
/// out, and neither path can avoid either. So the choice was made on the other
/// axis: `textContent` cannot produce an injection bug, and there is no escape
/// pass to get wrong the day the server starts sending something new.
export function appendRows(ol, lines, from, to) {
  // Off-DOM, so the `<ol>` takes one insertion and one layout rather than one
  // per row.
  const rows = document.createDocumentFragment()
  for (let at = from; at < to; at++) {
    const line = lines[at]
    const row = document.createElement("li")
    for (let n = 0; n < line.length; n++) {
      const chunk = line[n]
      const span = document.createElement("code")
      // `kind` is a capture name from `crate::highlight`, never anything out of
      // the file being read.
      if (chunk.kind) span.className = `hl-${chunk.kind}`
      span.textContent = chunk.text
      row.appendChild(span)
    }
    rows.appendChild(row)
  }
  ol.appendChild(rows)
}

/// Sizes the spacer that stands in for rows that have not been built yet, so
/// the scrollbar is the length of the file from the first frame rather than the
/// length of its first screen.
///
/// Sized from the rows that *will be rendered*, never from the file's own line
/// count: they differ exactly when a file was cut at 512 KB, and a spacer on
/// the file's count would leave a truncated file with a scrollbar three times
/// longer than its content.
function spaceFor(spacer, rows) {
  if (rows > 0) {
    spacer.style.setProperty("--rows", String(rows))
    spacer.hidden = false
  } else {
    spacer.hidden = true
    spacer.style.removeProperty("--rows")
  }
}

/// Paints a whole file into a fresh `<ol>`: a screenful now, the rest on the
/// next frame.
///
/// Two passes for the reason they were always two — something to read arrives
/// in a third of the time — but no longer *only* two out of necessity. The tail
/// is one append because appending is linear and one long append is the
/// cheapest way to finish; splitting it further is now a one-line change rather
/// than a template rewrite, and the measurement says it is not worth making
/// today.
///
/// Returns nothing. Cancellation is the caller's: it passes `alive`, which is
/// asked again on the next frame, because the rows belong to the tab on screen
/// and never to the one that was.
export function paintFile(ol, spacer, lines, alive) {
  ol.replaceChildren()
  const head = Math.min(FIRST_ROWS, lines.length)
  appendRows(ol, lines, 0, head)
  if (head === lines.length) return spaceFor(spacer, 0)
  spaceFor(spacer, lines.length - head)
  requestAnimationFrame(() => {
    if (!alive()) return
    appendRows(ol, lines, head, lines.length)
    spaceFor(spacer, 0)
  })
}
