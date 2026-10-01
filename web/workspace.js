// @ts-check
/// The workspace panels' shared state: which folder is being looked at, what
/// the tree has open, what git says, and what the content column is showing.
///
/// A module rather than props threaded through `app.html`, for the reason
/// `store.js` is one: the Files panel, the Git panel and the content viewer are
/// siblings, and a parent whose only job is relaying between its children is a
/// parent the state does not need. Both panels write tabs; the viewer reads
/// them. See `RECORD/2026-09-15.a-three-pane-inspector.completed.md` and
/// `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.

import { $reactive } from "./vendor/jq79.js"
import { apiHeaders } from "./store.js"
import { closing } from "./modal.js"
import { setPane } from "./prefs.js"

export const workspace = $reactive({
  /// The absolute path `luu serve` was started in, as the server reports it.
  /// Display only — nothing is ever asked for by absolute path, because an
  /// absolute path in a URL is an invitation to send a different one.
  base: "",
  /// The chosen folder, relative to the base and `/`-separated. `""` is the
  /// base itself. **`null` means nobody has chosen yet**, which is what opens
  /// the picker over the whole window on a first visit — it is not the same
  /// state as `""`, and collapsing the two would mean the picker either never
  /// appears or appears every time.
  root: null,
  /// One entry per directory that has been opened, keyed by its path relative
  /// to the sandbox base — never to the root, so narrowing the root does not
  /// invalidate a listing that is still correct. `""` is the base itself.
  /// Lazy, like every file tree: a repository with a `target/` in it is not
  /// something to walk eagerly.
  open: {},
  /// Directories currently expanded, in the order they were opened. A plain
  /// object beside `open` rather than a flag inside it: collapsing a directory
  /// should not throw away what was already fetched.
  expanded: {},
  /// What the content column is holding: `{ id, kind, title, path, staged,
  /// preview }`, where `kind` is `file`, `diff` or `debug`. One tab per open
  /// thing, so looking at a diff no longer loses the file that was on screen;
  /// at most one of them a `preview`, which the next single click replaces.
  tabs: [],
  /// The active tab's id, or `null` for an empty column.
  active: null,
  /// The active tab itself. `picked` rather than `selected` in the components
  /// that read it, and the name is load-bearing: a scope variable called
  /// `selected` collides with the DOM property of that name, and jq79's render
  /// effect ends up writing what it reads. See `content-viewer.html`.
  selected: null,
  /// What the viewer is showing, once it has arrived.
  /// `{ kind: "file", path, lines, truncated }`,
  /// `{ kind: "diff", path, staged, hunks, note }` or
  /// `{ kind: "debug", text }`.
  content: null,
  /// Set while a fetch for the active tab is in flight, so the viewer shows
  /// nothing rather than the previous file under the new file's name. It no
  /// longer says "Reading…": a line that came and went on every click moved the
  /// file down and back up.
  loading: false,
  /// Every changed path in the whole base, mapped to git's own two-letter code.
  /// Asked for the base and filtered to the root in the page rather than
  /// parameterised: it is one `git status --porcelain` for a repository either
  /// way, and a second parameter would be a second thing to keep consistent
  /// with the prefix.
  status: {},
  /// Why git could not be asked, when it could not. A workspace that is not a
  /// git repository is an ordinary case and lands here as a plain sentence.
  gitError: null,
  /// The last failure from any of these endpoints, for the panel to show.
  error: null,
  /// The icon theme's maps, by icon id, or `{ loaded: false }` when this
  /// machine named none. See `crate::icons` for why nothing is vendored, and
  /// `iconFor` for why the tree asks it per row rather than reading this flag.
  icons: { loaded: false },
  /// The folder picker: whether it is open, and the directory it is showing.
  /// `forced` is a first visit, where it covers the window and has no way out
  /// other than choosing — there is nothing behind it to go back to.
  picker: { open: false, forced: false, at: "" },
})

// ---- which folder ---------------------------------------------------------
//
// The folder `luu serve` was started in is the **ceiling**, and what is chosen
// is a subdirectory of it. Nothing here widens anything: `/api/workspace/*`
// resolves every path through `Sandbox::check_path` against that base, and a
// chosen root is a prefix this page puts on what it asks for. A picker that
// could climb above the base would be a browser control that moves the sandbox,
// and that is the one thing the workspace surface has never been.

/// Where the remembered root is kept, keyed by the base — two servers on one
/// machine do not inherit each other's answer.
const rootKey = base => `luu.root:${base}`

/// A base-relative path as the panels *show* it — relative to the chosen root.
/// Returns `null` for anything outside it, which is what filters git's status
/// down to the folder somebody chose.
///
/// **Only the display goes through here.** Every path this module sends to the
/// server stays relative to the base, exactly as the server hands it out: a
/// second spelling for the same file is a second chance to get the prefix wrong,
/// and the root is a narrowing of what is *listed*, not a second address space.
export function inside(path) {
  const root = workspace.root || ""
  if (!root) return path
  if (path === root) return ""
  return path.startsWith(`${root}/`) ? path.slice(root.length + 1) : null
}

/// Reads the base off the settings the server already answers, and the root off
/// what this browser remembered. Opens the picker when nothing was.
export async function loadRoot(base) {
  workspace.base = base || ""
  let remembered = null
  try {
    remembered = localStorage.getItem(rootKey(workspace.base))
  } catch {
    // A private window. The picker opens; the choice is simply not kept.
  }
  if (remembered === null) {
    workspace.picker = { open: true, forced: true, at: "" }
    return false
  }
  workspace.root = remembered
  return true
}

export function openPicker() {
  workspace.picker = { open: true, forced: false, at: workspace.root || "" }
  // The picker lists directories, which means listing them: a folder nobody has
  // opened in the tree has never been fetched.
  openDir(workspace.picker.at)
}

export function closePicker() {
  // A first visit has nothing behind it to go back to, so the only way out of a
  // forced picker is choosing.
  if (workspace.picker.forced) return
  closing("folder-dialog", () => { workspace.picker = { open: false, forced: false, at: "" } })
}

export function browseTo(path) {
  workspace.picker = { ...workspace.picker, at: path }
  openDir(path)
}

/// Takes the folder, and starts the panels over on it. Everything that was open
/// was open relative to a different root, so none of it survives: a tab holding
/// a file outside the new root would be a tab the tree cannot reach.
export async function chooseRoot(path) {
  workspace.root = path
  try {
    localStorage.setItem(rootKey(workspace.base), path)
  } catch {
    // Applied for this visit, and asked again next time.
  }
  workspace.picker = { open: false, forced: false, at: "" }
  workspace.tabs = []
  workspace.active = null
  workspace.selected = null
  workspace.content = null
  workspace.open = {}
  workspace.expanded = {}
  await openDir(path, true)
  await loadStatus()
}

/// The icon id for one entry, by VSCode's own rules.
///
/// Full filename first, then progressively shorter extensions — so `d.ts`
/// beats `ts` for `index.d.ts`, which is the distinction those themes draw —
/// then the theme's default. `null` when this row has no icon in this theme —
/// no theme loaded, or a theme that declares no default — and the tree draws
/// its own file or folder instead.
export function iconFor(name, isDir, expanded = false) {
  const theme = workspace.icons
  if (!theme.loaded) return null
  const lower = name.toLowerCase()
  if (isDir) {
    return (
      theme.folder_names?.[lower] ??
      (expanded ? theme.folder_expanded : theme.folder) ??
      theme.folder ??
      null
    )
  }
  const named = theme.file_names?.[lower]
  if (named) return named
  const parts = lower.split(".")
  for (let at = 1; at < parts.length; at++) {
    const suffix = parts.slice(at).join(".")
    const found = theme.file_extensions?.[suffix]
    if (found) return found
  }
  return theme.file ?? null
}

export async function loadIcons() {
  try {
    workspace.icons = await ask("./api/icons/manifest")
  } catch {
    // A page that cannot read the manifest simply has no icons; the tree is
    // the point and it still lists.
    workspace.icons = { loaded: false }
  }
}

/// `[ui] icon-theme` as Settings shows it: what is set, whether it loaded, and
/// every theme this machine has. See
/// `RECORD/2026-09-28.an-icon-theme-from-settings.completed.md`.
export async function loadIconTheme() {
  return ask("./api/icon-theme")
}

/// Sets the theme to a path (or clears it with `null`), and redraws the tree's
/// icons once the server has swapped it. Resolves to `{ ok, view }` or
/// `{ ok: false, error }`: a path that does not load is refused with the
/// loader's own message, which is the one worth showing.
export async function saveIconTheme(path) {
  return answered(await fetch("./api/icon-theme", {
    method: "PUT",
    headers: { ...apiHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ icon_theme: path }),
  }))
}

/// Imports the folder a directory picker chose, then selects it. `files` is
/// the input's `FileList`; each one goes up under the path the picker gave it.
export async function importIconTheme(files) {
  const form = new FormData()
  for (const file of files) form.append("file", file, file.webkitRelativePath || file.name)
  return answered(await fetch("./api/icon-theme/import", {
    method: "POST",
    headers: apiHeaders(),
    body: form,
  }))
}

async function answered(response) {
  if (!response.ok) return { ok: false, error: (await response.text()) || `HTTP ${response.status}` }
  const view = await response.json()
  await loadIcons()
  return { ok: true, view }
}

async function ask(url) {
  const answer = await fetch(url, { headers: apiHeaders() })
  if (!answer.ok) throw new Error((await answer.text()) || `HTTP ${answer.status}`)
  return answer.json()
}

/// Lists one directory, caching it. `force` re-reads one already listed, which
/// is what the refresh button wants. `path` is relative to the base.
export async function openDir(path = "", force = false) {
  const key = path || ""
  if (!force && workspace.open[key]) {
    workspace.expanded = { ...workspace.expanded, [key]: true }
    return
  }
  try {
    const tree = await ask(`./api/workspace/tree?path=${encodeURIComponent(key)}`)
    workspace.open = { ...workspace.open, [key]: tree.entries }
    workspace.expanded = { ...workspace.expanded, [key]: true }
    workspace.gitError = tree.git || null
    workspace.error = null
  } catch (e) {
    workspace.error = `Could not list ${key || "the workspace"}: ${e.message}`
  }
}

export function closeDir(path) {
  const next = { ...workspace.expanded }
  delete next[path]
  workspace.expanded = next
}

export function toggleDir(path) {
  if (workspace.expanded[path]) closeDir(path)
  else openDir(path)
}

/// Re-reads every directory that is currently open, and git with it. What the
/// refresh button does: a tree is a snapshot, and an agent that just wrote a
/// file has made it stale.
export async function refresh() {
  const keys = Object.keys(workspace.open)
  await Promise.all(keys.map(key => openDir(key, true)))
  await loadStatus()
}

export async function loadStatus() {
  try {
    workspace.status = await ask("./api/workspace/git-status")
    workspace.gitError = null
  } catch (e) {
    workspace.status = {}
    workspace.gitError = e.message
  }
}

// ---- the content column's tabs --------------------------------------------
//
// **A tab holds its identity, not its bytes.** Activating one re-fetches. The
// server answers the largest file in this repository in 20 ms and a small one in
// 0.6; the 271 ms a file costs is building the rows, which a cache would not
// save because the rows are rebuilt on activation either way. So a cache would
// hold megabytes of JSON to save 20 ms of the 291 that switching a tab costs.
// See `RECORD/2026-09-16.what-the-debug-ui-does-not-need.completed.md`.

const fileId = path => `file:${path}`
// **One tab per file's diff, not one per side.** "Staged" and "working tree"
// are two different answers about the same file, and the foot's toggle is how
// you read the other one — a second tab with the same name in it would make the
// strip say there are two files open when there is one.
const diffId = path => `diff:${path}`

const basename = path => path.split("/").pop() || path

/// Opens a tab, or focuses the one already holding this thing, and points the
/// second column at the content — the click already said which column the
/// person wants, and in the two-column layout having nothing happen is the bug
/// this rule exists to prevent.
///
/// **A single click opens a preview, the way VS Code does.** There is one
/// preview tab at most, drawn in italics, and the next single click replaces
/// it in place rather than adding a tab beside it: looking through ten files
/// should not leave ten tabs to close. A double click, the tree's pencil, or a
/// double click on the tab itself pins it ([`pinTab`]). Reopening a tab that is
/// already pinned never unpins it — the foot's "staged" toggle calls this with
/// the same id.
function openTab(tab, { preview = false } = {}) {
  const found = workspace.tabs.find(open => open.id === tab.id)
  // Replaced rather than mutated, the way every reactive list in this page is:
  // jq79 does not wake an `:each` when a property of an object inside a
  // reactive array is assigned from outside the component. See `store.js`.
  if (found) {
    const next = { ...found, ...tab, preview: found.preview && preview }
    workspace.tabs = workspace.tabs.map(open => open.id === tab.id ? next : open)
  } else {
    const next = { ...tab, preview }
    const old = preview ? workspace.tabs.findIndex(open => open.preview) : -1
    workspace.tabs = old < 0
      ? [...workspace.tabs, next]
      : workspace.tabs.map((open, at) => at === old ? next : open)
  }
  setPane("content")
  return activate(tab.id)
}

/// Keeps a preview tab: the next single click opens beside it instead of over
/// it. Also what an edit should call, once this column can edit — the viewer
/// is read-only today, so nothing does yet.
export function pinTab(id) {
  const found = workspace.tabs.find(open => open.id === id)
  if (!found?.preview) return
  workspace.tabs = workspace.tabs.map(open => open.id === id ? { ...open, preview: false } : open)
}

export function closeTab(id) {
  const at = workspace.tabs.findIndex(tab => tab.id === id)
  if (at < 0) return
  const left = workspace.tabs.filter(tab => tab.id !== id)
  workspace.tabs = left
  if (workspace.active !== id) return
  // Falls to the neighbour on the right, then on the left: closing the tab you
  // are looking at should land you next to it rather than at the far end.
  const next = left[at] || left[at - 1] || null
  if (next) {
    activate(next.id)
  } else {
    workspace.active = null
    workspace.selected = null
    workspace.content = null
  }
}

export function activate(id) {
  const tab = workspace.tabs.find(open => open.id === id)
  if (!tab) return
  workspace.active = id
  // A **copy**, never the element out of `tabs`. Putting the same object at two
  // paths in a reactive store means every read of `selected` re-wraps what is
  // already wrapped under `tabs`, the identity changes on every pass, and jq79
  // gives up after 100 of them with "an effect re-woke itself" — which names
  // neither this line nor this file. Copying is what makes the two paths two
  // values. See `RECORD/2026-09-16.three-columns-that-each-have-a-footer.completed.md`.
  workspace.selected = { ...tab }
  if (tab.kind === "file") return loadFile(tab)
  if (tab.kind === "diff") return loadDiff(tab)
  // A debug tab carries its own text: there is nothing to fetch, because the
  // context panel already had it.
  workspace.content = { kind: "debug", text: tab.text, path: tab.title }
  workspace.loading = false
}


/// `pinned` is the double click and the pencil; anything else is a preview.
export function showFile(path, { pinned = false } = {}) {
  return openTab(
    { id: fileId(path), kind: "file", path, title: basename(path), staged: false },
    { preview: !pinned },
  )
}

export function showDiff(path, staged = false, { pinned = false } = {}) {
  return openTab(
    { id: diffId(path), kind: "diff", path, staged, title: basename(path) },
    { preview: !pinned },
  )
}

/// A prompt, a trace, a tool's output: text the context panel already has, in
/// the column that is wide enough to read it. The one thing a 20rem rail has
/// never been able to show.
export function showDebug(id, title, text) {
  return openTab({ id: `debug:${id}`, kind: "debug", path: title, title, text })
}

async function loadFile(tab) {
  workspace.loading = true
  try {
    const file = await ask(`./api/workspace/file?path=${encodeURIComponent(tab.path)}`)
    // Dropped if the person clicked something else while this was in flight:
    // the column's tab and its body have to be the same file.
    if (workspace.active !== tab.id) return
    // The rows themselves are not stored: `content.lines` is what the viewer
    // paints from, and `rows.js` owns the two passes that put them on screen.
    // They used to be three reactive fields here — `head`, `tail` and
    // `pending` — and all three existed only to work around a `:each` that
    // re-derives its list rather than appending to it. See
    // `RECORD/2026-09-16.the-viewer-in-plain-js.completed.md`.
    workspace.content = { kind: "file", ...file }
    workspace.error = null
  } catch (e) {
    workspace.content = null
    workspace.error = `Could not read ${tab.path}: ${e.message}`
  } finally {
    if (workspace.active === tab.id) workspace.loading = false
  }
}

async function loadDiff(tab) {
  workspace.loading = true
  try {
    const diff = await ask(
      `./api/workspace/git-diff?path=${encodeURIComponent(tab.path)}&staged=${tab.staged}`,
    )
    if (workspace.active !== tab.id) return
    workspace.content = { kind: "diff", ...diff }
    workspace.error = null
  } catch (e) {
    workspace.content = null
    workspace.error = `Could not diff ${tab.path}: ${e.message}`
  } finally {
    if (workspace.active === tab.id) workspace.loading = false
  }
}
