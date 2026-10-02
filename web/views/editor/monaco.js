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
      defineLanguages(window.monaco)
      return window.monaco
    })().catch(() => null)
  }
  return loading
}

/// Monokai in its two tones, the ones `app.css` draws this page's viewer and a
/// reply's snippets in: Sublime Text 3's own on its dark ground, and the same
/// hues darkened to read at 5:1 on white. Written out rather than read off the
/// stylesheet, because Monaco wants them before anything is on screen and
/// they are fixed colours, not the page's.
function defineThemes(monaco) {
  const tone = ({ base, ground, ink, gutter, line, selection, red, yellow, grey, purple, green, blue }) => ({
    base,
    inherit: true,
    rules: [
      { token: "", foreground: ink },
      { token: "keyword", foreground: red },
      { token: "operator", foreground: red },
      { token: "tag", foreground: red },
      { token: "string", foreground: yellow },
      { token: "attribute.value", foreground: yellow },
      { token: "comment", foreground: grey, fontStyle: "italic" },
      { token: "number", foreground: purple },
      { token: "constant", foreground: purple },
      { token: "type", foreground: blue, fontStyle: "italic" },
      { token: "attribute.name", foreground: green },
      { token: "function", foreground: green },
      { token: "delimiter", foreground: ink },
    ],
    colors: {
      "editor.background": `#${ground}`,
      "editor.foreground": `#${ink}`,
      "editor.lineHighlightBackground": `#${line}`,
      "editor.selectionBackground": `#${selection}`,
      "editorCursor.foreground": `#${ink}`,
      "editorLineNumber.foreground": `#${gutter}`,
      "editorGutter.background": `#${ground}`,
    },
  })
  monaco.editor.defineTheme("luu-monokai", tone({
    base: "vs-dark", ground: "111111", ink: "f8f8f2", gutter: "90908a", line: "3e3d32", selection: "49483e",
    red: "f92672", yellow: "e6db74", grey: "75715e", purple: "ae81ff", green: "a6e22e", blue: "66d9ef",
  }))
  monaco.editor.defineTheme("luu-monokai-light", tone({
    base: "vs", ground: "ffffff", ink: "1a1d23", gutter: "7c8088", line: "f4f4ee", selection: "e6e4d4",
    red: "dc0653", yellow: "7a7016", grey: "736f5d", purple: "8541ff", green: "577a11", blue: "0f7a8e",
  }))
}

/// The server's own language names, which are `crate::highlight`'s, mapped to
/// Monaco's. Only where the two disagree — everything else is already the same
/// word, and a table that restated the agreements would be a table that drifts.
const LANGUAGES = { tsx: "typescript", bash: "shell", make: "makefile" }

/// The two languages `crate::highlight` colours and Monaco ships nothing for:
/// TOML and Make. Small Monarch grammars, emitting the token names the two
/// Monokai themes above already colour — so a key reads like an attribute, a
/// table header like a type, a target like a function — and nothing more: the
/// viewer is read-only, and this is colour, not a language service.
function defineLanguages(monaco) {
  monaco.languages.register({ id: "toml", extensions: [".toml"] })
  monaco.languages.setMonarchTokensProvider("toml", {
    tokenizer: {
      root: [
        [/#.*$/, "comment"],
        [/^\s*\[\[?[^\]]*\]\]?/, "type"],
        [/[A-Za-z0-9_\-.]+(?=\s*=)/, "attribute.name"],
        [/"""/, "string", "@basic"],
        [/'\'\'/, "string", "@literal"],
        [/"([^"\\]|\\.)*"/, "string"],
        [/'[^']*'/, "string"],
        [/\b(true|false)\b/, "keyword"],
        [/\d{4}-\d{2}-\d{2}([T ][\d:.]+)?(Z|[+-]\d{2}:\d{2})?/, "number"],
        [/[+-]?(0x[\da-fA-F_]+|0o[0-7_]+|0b[01_]+|\d[\d_]*(\.\d[\d_]*)?([eE][+-]?\d+)?|inf|nan)\b/, "number"],
        [/[=,{}\[\]]/, "delimiter"],
      ],
      basic: [[/"""/, "string", "@pop"], [/[^"]+|"/, "string"]],
      literal: [[/'\'\'/, "string", "@pop"], [/[^']+|'/, "string"]],
    },
  })
  monaco.languages.register({ id: "makefile", filenames: ["Makefile", "makefile", "GNUmakefile"], extensions: [".mk"] })
  monaco.languages.setMonarchTokensProvider("makefile", {
    tokenizer: {
      root: [
        [/#.*$/, "comment"],
        [/^(ifeq|ifneq|ifdef|ifndef|else|endif|include|-include|sinclude|define|endef|export|unexport|override|vpath)\b/, "keyword"],
        [/^\s*\.?[A-Za-z_][\w.]*(?=\s*(::=|:=|\?=|\+=|!=|=))/, "attribute.name"],
        [/^\.[A-Z]+(?=\s*:)/, "keyword"],
        [/^[^\s:#=][^:#=]*?(?=\s*::?(?!=))/, "function"],

        [/\$\(|\$\{/, "constant", "@reference"],
        [/\$[@<^+*?%$]/, "constant"],
        [/"([^"\\]|\\.)*"|'[^']*'/, "string"],
      ],
      reference: [
        [/[)}]/, "constant", "@pop"],
        [/\$\(|\$\{/, "constant", "@push"],
        [/[^)}$]+/, "constant"],
      ],
    },
  })
}

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
export async function paint(host, { text, language, tone, path }) {
  const monaco = await ensureMonaco()
  if (!monaco || !host) return false
  const theme = tone === "light" ? "luu-monokai-light" : "luu-monokai"
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
