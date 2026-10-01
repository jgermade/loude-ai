// @ts-check
/// Dragging the edge between two columns.
///
/// The inspector and the chat are the two columns with a width of their own;
/// the content column is whatever is left. Each width is a custom property on
/// `.app` (`--inspector-w`, `--chat-w`) that the grid reads, and the
/// stylesheet clamps it against the window — so this module only ever says
/// *how wide somebody asked for*, never how wide the window lets it be.
///
/// **A drag writes the property and nothing reactive.** A pointer move is
/// sixty a second, and a write to `prefs` wakes every line in the page that
/// read it; the width goes to `prefs` (and `localStorage`) once, when the
/// pointer is let go. See `RECORD/2026-10-01.columns-that-are-dragged.completed.md`.

import { prefs, setWidth } from "./prefs.js"

/// The narrowest either column is dragged to. The stylesheet has its own
/// floor too; this one is what the drag stops at, so the handle stays under
/// the pointer instead of running ahead of a column that will not follow.
const MIN = { inspector: 180, chat: 280 }
/// What the arrow keys move it by.
const STEP = 16

/// @param {"inspector" | "chat"} column
function property(column) {
  return column === "chat" ? "--chat-w" : "--inspector-w"
}

/// @param {"inspector" | "chat"} column
function current(column) {
  const col = document.querySelector(`.app > .col.${column}`)
  return col ? col.getBoundingClientRect().width : 0
}

/// The widest a column may be, the same rule the stylesheet's `clamp` keeps:
/// 35% of the window for the inspector, 50% for the chat.
/// @param {"inspector" | "chat"} column
function widest(column) {
  return window.innerWidth * (column === "chat" ? 0.5 : 0.35)
}

/// @param {"inspector" | "chat"} column
/// @param {number} px
function clamp(column, px) {
  return Math.max(MIN[column], Math.min(widest(column), px))
}

/// @param {"inspector" | "chat"} column
/// @param {number} px
function show(column, px) {
  /** @type {HTMLElement | null} */
  const app = document.querySelector(".app")
  app?.style.setProperty(property(column), `${Math.round(px)}px`)
}

/// `pointerdown` on a handle. The inspector grows to the right, the chat to the
/// left, so the same movement means opposite things to the two of them.
/// @param {PointerEvent} event
/// @param {"inspector" | "chat"} column
export function startDrag(event, column) {
  if (event.button !== 0) return
  event.preventDefault()
  /** @type {HTMLElement} */
  const handle = /** @type {HTMLElement} */ (event.currentTarget)
  handle.setPointerCapture(event.pointerId)
  const from = event.clientX
  const start = current(column)
  let width = start
  document.documentElement.classList.add("resizing")

  /** @param {PointerEvent} move */
  const onMove = move => {
    const moved = move.clientX - from
    width = clamp(column, column === "chat" ? start - moved : start + moved)
    show(column, width)
  }
  const onUp = () => {
    handle.removeEventListener("pointermove", onMove)
    handle.removeEventListener("pointerup", onUp)
    handle.removeEventListener("pointercancel", onUp)
    document.documentElement.classList.remove("resizing")
    if (Math.round(width) !== Math.round(start)) setWidth(column, width)
  }
  handle.addEventListener("pointermove", onMove)
  handle.addEventListener("pointerup", onUp)
  handle.addEventListener("pointercancel", onUp)
}

/// Arrow keys on a focused handle, for the same reason a slider takes them:
/// the handle is a control, and a control a mouse can work a keyboard can too.
/// @param {KeyboardEvent} event
/// @param {"inspector" | "chat"} column
export function nudge(event, column) {
  const sign = { ArrowLeft: -1, ArrowRight: 1 }[event.key]
  if (!sign) return
  event.preventDefault()
  // Right widens the inspector and narrows the chat: the edge moves the way
  // the arrow points, whichever column is on which side of it.
  const delta = column === "chat" ? -sign * STEP : sign * STEP
  const width = clamp(column, current(column) + delta)
  show(column, width)
  setWidth(column, width)
}

/// Double click: back to the stylesheet's width, and forgotten.
/// @param {"inspector" | "chat"} column
export function resetWidth(column) {
  /** @type {HTMLElement | null} */
  const app = document.querySelector(".app")
  app?.style.removeProperty(property(column))
  setWidth(column, null)
}

/// The style `.app` carries for what is remembered, for the template to bind.
/// @returns {string}
export function widthStyle() {
  return [
    prefs.inspectorWidth != null ? `--inspector-w:${prefs.inspectorWidth}px` : "",
    prefs.chatWidth != null ? `--chat-w:${prefs.chatWidth}px` : "",
  ].filter(Boolean).join(";")
}
