// @ts-check
/// The terminal panel's shell: one per page, where the live session's commands
/// run — its container, or this machine. And the place to move the session:
/// the panel's environment picker. See
/// `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
///
/// **Its life is here, not in the panel.** Hiding the panel moves xterm's
/// element out of the page and leaves the shell running, the way closing an
/// editor's terminal pane does; only *end* (or `exit`) ends it. A component
/// has no unmount hook to end it from, and a shell that died every time the
/// two-column layout swapped to the chat would be a shell nobody could use.
/// See `RECORD/2026-10-01.a-terminal-in-the-container.completed.md`.
///
/// xterm.js is an optional node dependency of `web/`, served from disk the way
/// Monaco is: `/api/terminal` says whether it is here.

import { $reactive } from "@web/vendor/jq79.js"
import { apiHeaders, socketUrl, refreshSettings } from "./store.js"
import { setPlace } from "./prefs.js"

export const terminal = $reactive({
  /// `{ available, place, reason, posture, xterm }` from `/api/terminal`, or
  /// `null` until asked. `place` is `host` or `container`; `reason` is why
  /// this request gets no shell.
  info: null,
  /// While the session is being moved to another posture, and why the last
  /// move was refused.
  moving: false,
  moveError: null,
  /// `idle` (no shell), `connecting`, `open`, or `ended` (the shell exited,
  /// or the container it was in went away).
  status: "idle",
})

/// xterm's own objects, outside the reactive store: they are a renderer's
/// state, and wrapping them in a proxy would wake effects on every keystroke.
let term = null
let fit = null
let socket = null
let observer = null

const encoder = new TextEncoder()

/// What the live session allows, asked again whenever the session may have
/// changed.
export async function loadTerminal() {
  try {
    const answer = await fetch("./api/terminal", { headers: apiHeaders() })
    terminal.info = answer.ok ? await answer.json() : { available: false, reason: `HTTP ${answer.status}` }
  } catch (e) {
    terminal.info = { available: false, reason: e.message, xterm: false }
  }
}

const vendor = path => new URL(`./vendor/xterm/${path}`, document.baseURI).href

let loaded = null
function xterm() {
  loaded ??= (async () => {
    const link = document.createElement("link")
    link.rel = "stylesheet"
    link.href = vendor("xterm/css/xterm.css")
    document.head.appendChild(link)
    const [{ Terminal }, { FitAddon }] = await Promise.all([
      import(vendor("xterm/lib/xterm.mjs")),
      import(vendor("addon-fit/lib/addon-fit.mjs")),
    ])
    return { Terminal, FitAddon }
  })()
  return loaded
}

/// The sixteen a shell names, as xterm calls them and as the panel's tokens
/// do (`--ansi-yellow`, `--ansi-bright-yellow`).
const ANSI = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]

/// The panel's own colours, read where xterm draws: the panel is dark whatever
/// the page is (`app.css`), so the page's root is not where to ask. A token
/// the panel does not set is left out, and xterm keeps its own colour for it.
function theme(host) {
  const css = getComputedStyle(host)
  const token = name => css.getPropertyValue(name).trim()
  const palette = {}
  for (const name of ANSI) {
    const bright = `bright${name[0].toUpperCase()}${name.slice(1)}`
    if (token(`--ansi-${name}`)) palette[name] = token(`--ansi-${name}`)
    if (token(`--ansi-bright-${name}`)) palette[bright] = token(`--ansi-bright-${name}`)
  }
  return {
    ...palette,
    background: token("--bg"),
    foreground: token("--fg"),
    cursor: token("--accent"),
    selectionBackground: token("--selected") || token("--line"),
  }
}

function send(bytes) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(bytes)
}

function sendSize() {
  if (term) send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }))
}

function connect() {
  terminal.status = "connecting"
  const opened = new WebSocket(socketUrl("/ws/terminal"))
  opened.binaryType = "arraybuffer"
  socket = opened
  opened.onopen = () => {
    terminal.status = "open"
    sendSize()
  }
  opened.onmessage = event => {
    term?.write(typeof event.data === "string" ? event.data : new Uint8Array(event.data))
  }
  opened.onclose = () => {
    if (socket !== opened) return
    socket = null
    terminal.status = "ended"
    term?.write("\r\n\x1b[2m[the terminal closed]\x1b[0m\r\n")
  }
}

/// Puts the shell in `host`, starting one if there is none. Called by the
/// panel every time it is drawn.
export async function attach(host) {
  if (!terminal.info?.available || !terminal.info?.xterm) return
  const { Terminal, FitAddon } = await xterm()
  if (!host.isConnected) return
  if (!term) {
    term = new Terminal({
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
      fontSize: 12,
      cursorBlink: true,
      theme: theme(host),
    })
    fit = new FitAddon()
    term.loadAddon(fit)
    term.onData(data => send(encoder.encode(data)))
    term.onResize(sendSize)
    term.open(host)
    connect()
  } else {
    term.options.theme = theme(host)
    if (term.element && term.element.parentElement !== host) host.appendChild(term.element)
  }
  observer?.disconnect()
  observer = new ResizeObserver(() => {
    if (host.isConnected && host.clientWidth) fit?.fit()
  })
  observer.observe(host)
  fit.fit()
  term.focus()
}

/// A new shell in the same panel, after the last one ended.
export function restartTerminal() {
  if (!term) return
  socket?.close()
  socket = null
  term.reset()
  connect()
}

/// Ends the shell for good. The next time the panel is drawn starts another.
export function endTerminal() {
  observer?.disconnect()
  observer = null
  const closing = socket
  socket = null
  closing?.close()
  term?.dispose()
  term = null
  fit = null
  terminal.status = "idle"
}

/// Moves the live session to another posture — `""` is the server's own — on
/// `runtime` where that is a variant (`null` is the posture's own), and opens
/// the shell again where it now runs.
///
/// The server refuses it while a job is open or a proposal waits at the gate,
/// and says which; that sentence is what the panel shows. The shell that was
/// open is ended either way the session moved: in a container it is going
/// with the container, and on this machine it would be a shell somewhere the
/// session no longer runs.
export async function moveSession(name, runtime, host) {
  terminal.moving = true
  terminal.moveError = null
  try {
    const answer = await fetch("./api/session/posture", {
      method: "PUT",
      headers: { ...apiHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify({ posture: name || null, runtime: runtime || null }),
    })
    if (!answer.ok) {
      terminal.moveError = (await answer.text()) || `HTTP ${answer.status}`
      // Asked again so the picker goes back to where the session still is.
      await loadTerminal()
      return
    }
    const { moved } = await answer.json()
    if (moved) {
      // The next session is offered where this one now runs.
      setPlace(`${name || ""}|${runtime || ""}`)
      endTerminal()
      await refreshSettings()
    }
    await loadTerminal()
    if (host?.isConnected) await attach(host)
  } catch (e) {
    terminal.moveError = e.message
  } finally {
    terminal.moving = false
  }
}
