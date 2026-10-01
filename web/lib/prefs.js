// @ts-check
/// Everything the page remembers about *this browser*, in one module.
///
/// None of it is in `config.toml` and none of it is sent anywhere, for the
/// reason the theme was never in it: a theme, a layout and which editor draws a
/// file are facts about the screen somebody is reading from, and one server
/// answers two people on two machines. `config.toml` is where a *destination*
/// is written down, which is a fact about the run.
///
/// A module rather than scope variables in `app.html`, for the reason
/// `store.js` and `workspace.js` are modules: the settings modal writes these
/// and the shell reads them, and they are siblings. See
/// `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.

import { $reactive } from "@web/vendor/jq79.js"
import { apiHeaders } from "./store.js"

/// Reads one remembered value, falling back where the browser will not answer —
/// a private window, or site data blocked. Remembering is a convenience here,
/// never a requirement: every one of these has a working default.
function kept(key, allowed, fallback) {
  try {
    const found = localStorage.getItem(key)
    return allowed.includes(found) ? found : fallback
  } catch {
    return fallback
  }
}

/// A remembered width in pixels, or `null` for the stylesheet's own. Anything
/// that is not a sane number is the default rather than a column a pixel wide.
function keptWidth(key) {
  try {
    const found = Number(localStorage.getItem(key))
    return Number.isFinite(found) && found >= 120 && found <= 4000 ? Math.round(found) : null
  } catch {
    return null
  }
}

function keep(key, value) {
  try {
    localStorage.setItem(key, value)
  } catch {
    // Unremembered, still applied.
  }
}

export const prefs = $reactive({
  /// `auto` | `light` | `dark`. **`dark` is what a browser with nothing
  /// remembered gets**, and deliberately: every screenshot and every record in
  /// this repo was taken against it, and a default that followed the OS would
  /// quietly reinterpret all of them. `auto` is a choice, not the absence of
  /// one.
  // Light only, for now: the dark theme is off while the page's palette is
  // reworked. Its tokens stay in `app.css`, because the columns' heads and
  // feet are drawn with them.
  theme: "light",
  /// `own` | `monaco`. What draws a file in the content column. `monaco` is an
  /// npm dependency of this directory rather than a payload in the tree, so it
  /// is only offered where somebody installed it — see `monacoAvailable`.
  editor: kept("luu.editor", ["own", "monaco"], "own"),
  /// `dark` | `light`. Code is drawn in Monokai everywhere, in one of its two
  /// tones: Sublime Text 3's own on its dark ground, or the same hues darkened
  /// to read on white. The editor — this page's viewer and Monaco alike — is
  /// dark unless somebody asks; a snippet in a reply sits in a light chat, so
  /// it is light unless somebody asks.
  editorTone: kept("luu.editor-tone", ["dark", "light"], "dark"),
  snippetTone: kept("luu.snippet-tone", ["dark", "light"], "light"),
  /// `responsive` | `two`. `responsive` is three columns above 1260px and two
  /// below; `two` pins the two-column layout at any width, which is what
  /// somebody on a wide screen who wants the chat wide is asking for.
  layout: kept("luu.layout", ["responsive", "two"], "responsive"),
  /// Which of content and chat the second column is showing, while there are
  /// only two. Remembered because it is a place somebody was looking.
  pane: kept("luu.pane", ["content", "chat"], "chat"),
  /// Which panel the inspector is showing. Was in `app.html`; it is the same
  /// kind of fact as the four above and belongs with them.
  inspector: kept("luu.inspector.mode", ["files", "git", "debug"], "debug"),
  /// `manual` | `auto`. Whether the page answers the job gate itself.
  ///
  /// **`auto` is a person deciding once instead of per job, and never the
  /// server running unapproved work.** The gate is still there, the proposal
  /// still arrives, and the approval is still an `approve_job` this page sends
  /// — what changes is who presses the button. The distinction has to stay
  /// visible, which is why this control sits in the composer's own second row
  /// rather than in a modal somebody set once and forgot.
  confirm: kept("luu.confirm", ["manual", "auto"], "manual"),
  /// The inspector's and the chat's widths in pixels, as last dragged, or
  /// `null` for the stylesheet's 20rem and 30rem. The stylesheet still clamps
  /// whatever is here against the window, so a width kept on a wide screen
  /// cannot squeeze the content column out of a narrow one.
  inspectorWidth: keptWidth("luu.width.inspector"),
  chatWidth: keptWidth("luu.width.chat"),
})

/// Dark is `:root`'s own palette in `app.css`, so light is the attribute. Set
/// unconditionally while the dark theme is off — see `prefs.theme`.
function paint() {
  document.documentElement.setAttribute("data-theme", "light")
}

function paintTones() {
  document.documentElement.setAttribute("data-editor-tone", prefs.editorTone)
  document.documentElement.setAttribute("data-snippet-tone", prefs.snippetTone)
}

paint()
paintTones()
export function setEditorTone(which) {
  prefs.editorTone = which
  keep("luu.editor-tone", which)
  paintTones()
}

export function setSnippetTone(which) {
  prefs.snippetTone = which
  keep("luu.snippet-tone", which)
  paintTones()
}

export function setEditor(which) {
  prefs.editor = which
  keep("luu.editor", which)
}

export function setLayout(which) {
  prefs.layout = which
  keep("luu.layout", which)
}

export function setPane(which) {
  prefs.pane = which
  keep("luu.pane", which)
}

export function setInspector(which) {
  prefs.inspector = which
  keep("luu.inspector.mode", which)
}

/// `null` forgets the width, which is what a double click on the handle asks.
export function setWidth(column, px) {
  const key = column === "chat" ? "chatWidth" : "inspectorWidth"
  prefs[key] = px == null ? null : Math.round(px)
  try {
    if (px == null) localStorage.removeItem(`luu.width.${column}`)
    else localStorage.setItem(`luu.width.${column}`, String(Math.round(px)))
  } catch {
    // Unremembered, still applied.
  }
}

export function setConfirm(which) {
  prefs.confirm = which
  keep("luu.confirm", which)
}

/// Whether Monaco is on this machine, asked once and cached.
///
/// It is a node dependency of `web/` and not a file in the tree, so
/// the honest answer for a checkout that ran `cargo build` and nothing else is
/// *no*. The setting then says so and stays on this page's own viewer — the
/// same shape `[ui] icon-theme` has, where the feature waits to be told it is
/// there rather than shipping a copy of itself. See the record.
/// A question the server answers, rather than a probe that reads a 404: a
/// missing file works as a test and logs a console error every time, on every
/// checkout that has no Monaco — which is most of them.
let asked = null
export function monacoAvailable() {
  if (!asked) {
    asked = fetch("./api/monaco", { headers: apiHeaders() })
      .then(answer => (answer.ok ? answer.json() : { installed: false }))
      .then(answer => !!answer.installed)
      // A static twin has no server behind it, and no Monaco either.
      .catch(() => false)
  }
  return asked
}
