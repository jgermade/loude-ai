// @ts-check
/// Monaco, when somebody installed it.
///
/// **This is an opt-in, and it does not reopen the measurement.**
/// `RECORD/2026-09-16.what-the-debug-ui-does-not-need.completed.md` asked
/// *should Monaco be the viewer* and answered no with numbers: at the size this
/// panel is asked for, this page's own viewer paints in a third of the time
/// over a seventh of the bytes, and the whole file stays findable by the
/// browser's own Ctrl+F. Nothing here disputes that and nothing here changes
/// the default. What it adds is that the own viewer cannot fold, edit or jump
/// to a symbol, and the day somebody wants one of those the answer should be a
/// setting rather than a rewrite.
///
/// It is a **node dependency of this directory**, gitignored like every other
/// `node_modules`, excluded from the `rust_embed` folder and served from disk
/// by `serve::monaco_asset`. A checkout that ran `cargo build` and nothing else
/// has no Monaco, and that is the honest state — the same shape
/// `[ui] icon-theme` has. See
/// `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.

/// Monaco ships as AMD and its loader claims the page's `require`/`define`.
/// Loaded once, and only when somebody chose it: a `<script>` in `index.html`
/// would cost every visit for a setting that is off by default.
let loading = null

function loadLoader() {
  return new Promise((done, fail) => {
    const tag = document.createElement("script")
    tag.src = "./vendor/monaco/loader.js"
    tag.onload = () => done()
    tag.onerror = () => fail(new Error("monaco's loader did not load"))
    document.head.appendChild(tag)
  })
}

/// The `monaco` namespace, or `null` where it is not installed.
///
/// `null` rather than a throw, because *not installed* is an ordinary answer
/// here and every caller's response to it is the same: keep the own viewer.
export function ensureMonaco() {
  if (!loading) {
    loading = (async () => {
      if (window.monaco) return window.monaco
      await loadLoader()
      const amd = window.require
      if (!amd) throw new Error("monaco's loader defined no require")
      amd.config({ paths: { vs: "./vendor/monaco" } })
      await new Promise(done => amd(["vs/editor/editor.main"], done))
      defineThemes(window.monaco)
      return window.monaco
    })().catch(() => null)
  }
  return loading
}

/// Two themes built from this page's own tokens, read off the live stylesheet
/// rather than written out again here.
///
/// The same rule the highlighter follows: the capture names map to the page's
/// palette, so the viewer follows the light/dark toggle without acquiring a
/// second theme system. A Monaco that shipped its own would be the one thing on
/// the page that did not change with the toggle.
function tokenOf(name, fallback) {
  const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim()
  // Monaco wants `rrggbb` with no `#`, and refuses anything else.
  const hex = value.replace("#", "")
  return /^[0-9a-fA-F]{6}$/.test(hex) ? hex : fallback
}

function defineThemes(monaco) {
  for (const [name, base] of [["luu-dark", "vs-dark"], ["luu-light", "vs"]]) {
    monaco.editor.defineTheme(name, {
      base,
      inherit: true,
      rules: [
        { token: "keyword", foreground: tokenOf("--hl-keyword", "bb9af7") },
        { token: "string", foreground: tokenOf("--hl-string", "9ece6a") },
        { token: "comment", foreground: tokenOf("--hl-comment", "6b7487"), fontStyle: "italic" },
        { token: "number", foreground: tokenOf("--hl-number", "ff9e64") },
        { token: "type", foreground: tokenOf("--hl-type", "2ac3de") },
        { token: "tag", foreground: tokenOf("--hl-tag", "f7768e") },
        { token: "attribute.name", foreground: tokenOf("--hl-attribute", "e0af68") },
      ],
      colors: {
        "editor.background": `#${tokenOf("--bg", "14161a")}`,
        "editor.foreground": `#${tokenOf("--fg", "e6e8ec")}`,
        "editorLineNumber.foreground": `#${tokenOf("--dim", "98a0ad")}`,
        "editorGutter.background": `#${tokenOf("--bg", "14161a")}`,
      },
    })
  }
  // Monokai as Sublime Text 3 ships it, for the `code` preference. Written out
  // rather than read off the stylesheet, because it is the one set of colours
  // that is not the page's.
  monaco.editor.defineTheme("luu-monokai", {
    base: "vs-dark",
    inherit: true,
    rules: [
      { token: "", foreground: "f8f8f2" },
      { token: "keyword", foreground: "f92672" },
      { token: "operator", foreground: "f92672" },
      { token: "tag", foreground: "f92672" },
      { token: "string", foreground: "e6db74" },
      { token: "attribute.value", foreground: "e6db74" },
      { token: "comment", foreground: "75715e", fontStyle: "italic" },
      { token: "number", foreground: "ae81ff" },
      { token: "constant", foreground: "ae81ff" },
      { token: "type", foreground: "66d9ef", fontStyle: "italic" },
      { token: "attribute.name", foreground: "a6e22e" },
      { token: "delimiter", foreground: "f8f8f2" },
    ],
    colors: {
      "editor.background": "#272822",
      "editor.foreground": "#f8f8f2",
      "editor.lineHighlightBackground": "#3e3d32",
      "editor.selectionBackground": "#49483e",
      "editorCursor.foreground": "#f8f8f0",
      "editorLineNumber.foreground": "#90908a",
      "editorGutter.background": "#272822",
      "editorIndentGuide.background": "#464741",
      "editorWhitespace.foreground": "#3b3a32",
    },
  })
}

/// The server's own language names, which are `crate::highlight`'s, mapped to
/// Monaco's. Only where the two disagree — everything else is already the same
/// word, and a table that restated the agreements would be a table that drifts.
const LANGUAGES = { tsx: "typescript", bash: "shell" }

let editor = null
let attachedTo = null
/// The file the editor is showing, so a re-read of it is told from a new one.
let paintedPath = null

/// Puts one file on screen, creating the editor the first time.
///
/// Reused rather than recreated per tab: Monaco's construction is the expensive
/// half, and a tab switch that disposed and rebuilt it would spend that cost
/// every time. `attachedTo` is the host it was built into, because the host
/// element is recreated whenever the column's body re-renders.
export async function paint(host, { text, language, dark, code, path }) {
  const monaco = await ensureMonaco()
  if (!monaco || !host) return false
  const theme = code === "monokai" ? "luu-monokai" : dark ? "luu-dark" : "luu-light"
  if (editor && attachedTo !== host) {
    editor.dispose()
    editor = null
  }
  if (!editor) {
    editor = monaco.editor.create(host, {
      value: text,
      language: LANGUAGES[language] || language || "plaintext",
      theme,
      // Read-only, because this panel is read-only: the file tree and the git
      // panel are a window onto the workspace, not a second way to change it —
      // every write still goes through the job gate.
      readOnly: true,
      automaticLayout: true,
      minimap: { enabled: false },
      scrollBeyondLastLine: false,
      fontSize: 12,
    })
    attachedTo = host
    paintedPath = path
    return true
  }
  monaco.editor.setTheme(theme)
  const model = editor.getModel()
  if (model) {
    monaco.editor.setModelLanguage(model, LANGUAGES[language] || language || "plaintext")
    // `setValue` rather than a new model: a model per tab is a leak unless
    // every one of them is disposed, and there is one file on screen.
    // The same file re-read because it changed on disk keeps its scroll and
    // cursor rather than jumping to the top; another file starts at its own.
    if (model.getValue() !== text) {
      const view = path === paintedPath ? editor.saveViewState() : null
      model.setValue(text)
      if (view) editor.restoreViewState(view)
    }
  }
  paintedPath = path
  return true
}

export function dispose() {
  editor?.dispose()
  editor = null
  attachedTo = null
  paintedPath = null
}

/// The file the server sent, as text.
///
/// The payload is one list of chunks per line — already cut and classified, and
/// never offsets, which would be Rust byte indices read as JavaScript UTF-16
/// ones. Monaco wants the string back, so this is where it is put back
/// together, and nowhere else: the own viewer never needs it.
export function textOf(lines) {
  return (lines || []).map(line => line.map(chunk => chunk.text).join("")).join("\n")
}
