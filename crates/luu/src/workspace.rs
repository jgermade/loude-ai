//! The workspace, as the person at the browser looks at it: a file tree, what
//! git says changed, and the contents of one file or one diff.
//!
//! **Read-only, and outside the job gate on purpose.** Every other way this
//! program touches the filesystem is a *model's* action — `read_file` and
//! `run_command` are tools, and a tool call is held until somebody approves the
//! job that proposed it. Nothing here is proposed by anything: a person clicked
//! a directory. Routing that through the approval flow would mean either
//! rubber-stamping a job nobody asked the model to make, or building a second
//! read path that skips the gate — and then there are two ways to read a file
//! and only one of them is audited. The read side of the session API
//! (`/api/sessions/*`) already answers plain ungated `GET`s for the same
//! reason. See `RECORD/2026-09-15.a-three-pane-inspector.completed.md`.
//!
//! What still bounds it is the sandbox: every path is resolved through
//! [`Sandbox::check_path`], so this surface can reach exactly what the session's
//! own policy grants and nothing above it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use agent_core::sandbox::{Access, Sandbox};
use serde::Serialize;
use tokio::process::Command;

/// How long any one `git` call may take before it is abandoned. A repository
/// on a dead network mount is the case this exists for: the panel showing
/// "could not read git" beats the page hanging on a click.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One entry in a directory listing.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Relative to the sandbox base, `/`-separated, and the id the page sends
    /// back to open it. Never absolute: an absolute path in a URL is an
    /// invitation to send a different one.
    pub path: String,
    pub name: String,
    pub dir: bool,
    /// Whether git ignores it. Shown dimmed rather than hidden: a person
    /// looking for `target/` wants to see that it is there and ignored, not to
    /// wonder whether the tree is broken.
    pub ignored: bool,
    /// Git's own two-letter status, when it has one — `M`, `??`, `A`, `D`.
    /// `None` is unchanged, which is most of a tree.
    pub status: Option<String>,
    /// For a directory whose only content is one directory, and so on down:
    /// that chain, `/`-separated (`vscode`, `b/c`). The page draws it as one
    /// row, `parent/nested`, the way an editor's compact folders do, and it can
    /// only do that before the parent is opened if the listing says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<String>,
    /// A directory with a `.git` of its own: a repository, or the file a
    /// worktree or a submodule leaves. The project picker marks it, because a
    /// folder somebody works in is usually one. One `stat` per directory.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub repo: bool,
}

/// How deep a chain of single directories is followed. A tree that nests one
/// directory in another this many times is a tree nobody reads row by row.
const ONLY_DEPTH: usize = 8;

/// The single directory `dir` holds, if it holds exactly one entry and that
/// entry is a directory. `.git` does not count, as in the listing. Stops at the
/// second entry, so a `target/` costs two reads, not a walk.
async fn only_dir(dir: &Path) -> Option<String> {
    let mut reader = tokio::fs::read_dir(dir).await.ok()?;
    let mut found = None;
    while let Ok(Some(item)) = reader.next_entry().await {
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        if found.is_some() || !item.file_type().await.ok()?.is_dir() {
            return None;
        }
        found = Some(name);
    }
    found
}

/// The chain of single directories under `dir`, if there is one.
async fn only_chain(dir: &Path) -> Option<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut at = dir.to_path_buf();
    while chain.len() < ONLY_DEPTH {
        let Some(name) = only_dir(&at).await else {
            break;
        };
        at.push(&name);
        chain.push(name);
    }
    (!chain.is_empty()).then(|| chain.join("/"))
}

/// One directory's children.
#[derive(Debug, Serialize)]
pub struct Tree {
    /// What was listed, relative to the base. Empty string is the base itself.
    pub path: String,
    pub entries: Vec<Entry>,
    /// What went wrong asking git, when something did. The tree still lists —
    /// a workspace that is not a git repository at all is an ordinary case,
    /// not an error, and it renders as a tree with nothing marked.
    pub git: Option<String>,
}

/// A file, read for display.
#[derive(Debug, Serialize)]
pub struct FileView {
    pub path: String,
    /// Which grammar highlighted it, or `None` for a file that got none —
    /// unknown extension, too big to parse, or a parse that failed. The page
    /// shows it beside the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<&'static str>,
    /// True when the file was cut at [`MAX_FILE_BYTES`]. The page says so
    /// rather than showing a truncated file as if it were whole.
    pub truncated: bool,
    /// How many lines the **file** has, which is `lines.len()` for everything
    /// that fits and more than it for everything that was cut.
    ///
    /// Not redundant, which is the only reason it is here: the read below
    /// holds the whole file before the cut, so this number is already in hand,
    /// and without it `truncated` is a shrug — the page can say *cut at
    /// 512 KB* but not how much of the file that leaves. It is also the one
    /// fact a reader would need up front if this body ever became a stream,
    /// which is why it sits above `lines` with the rest of them.
    pub total_lines: usize,
    /// One entry per line, each already cut into highlighted runs. Not the
    /// whole text plus offsets: see `crate::highlight`'s own first paragraph
    /// for why offsets across the wire are a bug waiting for a non-ASCII
    /// character.
    ///
    /// **Last on purpose.** `serde` writes a struct's fields in declaration
    /// order, so anything after this one sits behind the whole file — 681 KB
    /// of JSON for `serve.rs` — both for a person reading the response in
    /// devtools and for any reader that ever consumes this as a stream. The
    /// facts about the file come first; the file comes last.
    pub lines: Vec<Vec<crate::highlight::Chunk>>,
}

/// The cap on a file the viewer will render. A debug UI has no business
/// streaming a gigabyte into a browser, and the number is the same order as
/// the tool output cap the agent already lives under.
pub const MAX_FILE_BYTES: usize = 512 * 1024;

/// One line of a diff, already classified so the browser renders structure
/// rather than parsing text.
#[derive(Debug, Serialize)]
pub struct DiffLine {
    /// `add`, `del`, or `ctx`.
    pub kind: &'static str,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct Hunk {
    /// Git's own `@@ … @@` header, kept verbatim: it is the one line here a
    /// person may want to read as git wrote it.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Serialize)]
pub struct Diff {
    pub path: String,
    /// Staged against `HEAD` when `staged`, working tree against the index
    /// otherwise.
    pub staged: bool,
    pub hunks: Vec<Hunk>,
    /// Set when git answered but there was nothing to show — a file that is
    /// untracked has no diff, and saying so beats an empty panel.
    pub note: Option<String>,
}

/// Why a request could not be answered. Kept as one type because every one of
/// these becomes the same shape of HTTP error, and the caller should not have
/// to remember which variant maps to which status.
#[derive(Debug)]
pub enum Error {
    /// The path escaped the sandbox, or the sandbox does not grant it.
    Refused(String),
    /// It is not there.
    Missing(String),
    /// Something under it failed — a read, a spawn, a decode.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Refused(why) | Error::Missing(why) | Error::Failed(why) => write!(f, "{why}"),
        }
    }
}

/// Resolves a caller-supplied relative path against the base, through the
/// sandbox.
///
/// The sandbox is what makes this safe rather than any string handling here:
/// `check_path` resolves symlinks and `..` and answers against the policy's
/// roots, which is the same check `read_file` goes through.
fn resolve(sandbox: &Sandbox, relative: &str) -> Result<PathBuf, Error> {
    let relative = relative.trim_start_matches('/');
    let joined = match relative.is_empty() {
        true => sandbox.base().to_path_buf(),
        false => sandbox.base().join(relative),
    };
    let check = sandbox.check_path(&joined, Access::Read);
    if !check.verdict.allowed {
        return Err(Error::Refused(format!(
            "{relative}: {}",
            check.verdict.rule
        )));
    }
    Ok(check.path)
}

/// The path an entry is known by: relative to the base, `/`-separated.
///
/// `pub(crate)` for the gate panel's floor line, which shows the same paths to
/// the same person and must spell them the same way — see
/// `RECORD/2026-09-21.what-an-unapproved-turn-may-reach.completed.md`. It
/// answers `""` for the base itself, and a caller wanting a word for *here*
/// supplies it.
pub(crate) fn relative_of(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// One directory's children, with git's opinion of each attached.
///
/// Ignored entries are found by difference rather than by a second matcher:
/// `ignore`'s walker answers "what is *not* ignored" for this directory, and
/// `read_dir` answers "what is there". Everything in the second and not the
/// first is ignored — which gets nested `.gitignore` files, negations and the
/// global config right for free, because the walker already implements all of
/// that and a hand-rolled matcher on the root file alone would not.
pub async fn tree(sandbox: &Sandbox, relative: &str) -> Result<Tree, Error> {
    let dir = resolve(sandbox, relative)?;
    if !dir.is_dir() {
        return Err(Error::Missing(format!("{relative}: not a directory")));
    }

    let mut listed: Vec<(String, bool, PathBuf)> = Vec::new();
    let mut reader = tokio::fs::read_dir(&dir)
        .await
        .map_err(|error| Error::Failed(format!("{relative}: {error}")))?;
    while let Some(item) = reader
        .next_entry()
        .await
        .map_err(|error| Error::Failed(format!("{relative}: {error}")))?
    {
        let name = item.file_name().to_string_lossy().into_owned();
        // `.git` is machinery, not content. It is the one directory whose
        // contents nobody opens this panel to read, and it is enormous.
        if name == ".git" {
            continue;
        }
        let is_dir = item
            .file_type()
            .await
            .map(|kind| kind.is_dir())
            .unwrap_or(false);
        listed.push((name, is_dir, item.path()));
    }

    let unignored = not_ignored(&dir);
    // A workspace that is not a git repository still has a tree worth listing,
    // so git failing is a note on the answer rather than an error instead of
    // one: nothing is marked, and the panel says why.
    let (statuses, git_error) = match git_status(sandbox.base()).await {
        Ok(map) => (map, None),
        Err(error) => (BTreeMap::new(), Some(error.to_string())),
    };

    let mut chains = Vec::with_capacity(listed.len());
    for (_, is_dir, full) in &listed {
        chains.push(if *is_dir {
            let repo = tokio::fs::symlink_metadata(full.join(".git")).await.is_ok();
            (only_chain(full).await, repo)
        } else {
            (None, false)
        });
    }

    let mut entries: Vec<Entry> = listed
        .into_iter()
        .zip(chains)
        .map(|((name, dir_flag, full), (only, repo))| {
            let path = relative_of(sandbox.base(), &full);
            // A directory carries the loudest status under it, so a collapsed
            // tree still shows that something inside changed.
            let status = match dir_flag {
                false => statuses.get(&path).cloned(),
                true => {
                    let prefix = format!("{path}/");
                    statuses
                        .iter()
                        .find(|(key, _)| key.starts_with(&prefix))
                        .map(|(_, value)| value.clone())
                }
            };
            Entry {
                ignored: !unignored.contains(&name),
                path,
                name,
                dir: dir_flag,
                status,
                only,
                repo,
            }
        })
        .collect();

    // Directories first, then by name: the order a file tree is read in.
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));

    Ok(Tree {
        path: relative.trim_start_matches('/').to_string(),
        entries,
        git: git_error,
    })
}

/// The names in `dir` that git does *not* ignore.
///
/// Depth 1 and `parents(true)`: the walker has to read `.gitignore` files above
/// this directory to answer correctly for a subdirectory, and must not descend
/// below it.
fn not_ignored(dir: &Path) -> std::collections::HashSet<String> {
    ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .parents(true)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
        .filter_map(Result::ok)
        // The walker yields the root itself first; it is not one of its own
        // children.
        .filter(|entry| entry.path() != dir)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Every path git considers changed, mapped to its two-letter status.
///
/// `-z` because a filename may contain anything, newlines and quotes included,
/// and `--porcelain`'s default output escapes those in a format that then has
/// to be un-escaped. The NUL-separated form is the one that needs no parsing
/// of quoting rules.
pub async fn git_status(base: &Path) -> Result<BTreeMap<String, String>, Error> {
    let out = git(base, &["status", "--porcelain=v1", "-z"]).await?;
    let mut map = BTreeMap::new();
    let mut fields = out.split('\0').filter(|part| !part.is_empty());
    while let Some(record) = fields.next() {
        // `XY <path>`, and for a rename `XY <to>\0<from>` — the extra field is
        // consumed here so it is not read as the next record.
        //
        // **Both characters, untrimmed.** X is the index against `HEAD` and Y
        // the working tree against the index, so ` M` and `M ` are opposite
        // facts and the space carries which one this is. Trimming it made
        // every unstaged change look staged, and the diff panel asked git for
        // the staged side and got nothing.
        let (code, path) = match record.len() > 3 {
            true => (record[..2].to_string(), record[3..].to_string()),
            false => continue,
        };
        if code.starts_with('R') || code.starts_with('C') {
            let _ = fields.next();
        }
        map.insert(path, code);
    }
    Ok(map)
}

/// A file, for the middle column.
pub async fn file(sandbox: &Sandbox, relative: &str) -> Result<FileView, Error> {
    let path = resolve(sandbox, relative)?;
    if path.is_dir() {
        return Err(Error::Missing(format!("{relative}: is a directory")));
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Error::Missing(format!("{relative}: not found")),
            _ => Error::Failed(format!("{relative}: {error}")),
        })?;
    let truncated = bytes.len() > MAX_FILE_BYTES;
    // Counted over the whole file rather than over what is sent, and counted
    // the way the text is split below: `split('\n')` yields one more piece
    // than there are newlines, so a file with no newline at all is one line
    // and an empty file is one empty line.
    let total_lines = bytes.iter().filter(|byte| **byte == b'\n').count() + 1;
    let bytes = match truncated {
        true => &bytes[..MAX_FILE_BYTES],
        false => &bytes[..],
    };
    // Lossy rather than refused: a file with one bad byte in it is still worth
    // looking at, and the alternative is a panel that says nothing about a file
    // the tree says exists.
    let path = relative.trim_start_matches('/').to_string();
    let text = String::from_utf8_lossy(bytes).into_owned();
    let (language, lines) = crate::highlight::lines(&path, &text);
    Ok(FileView {
        path,
        language,
        truncated,
        total_lines,
        lines,
    })
}

/// One file's diff, parsed into hunks.
///
/// Parsed here rather than shipped as text, which is the call
/// `luu-design.md` already made for prompt diffs: "compute the diff in Rust and
/// send resolved spans". Git already produces a unified diff, so this parses
/// that rather than re-diffing the blobs — same destination, no second diff
/// implementation and no new dependency.
pub async fn diff(sandbox: &Sandbox, relative: &str, staged: bool) -> Result<Diff, Error> {
    // Resolved for the same reason a read is: a path that the sandbox refuses
    // must not become a `git diff` argument either.
    resolve(sandbox, relative)?;
    let path = relative.trim_start_matches('/');
    let mut args = vec!["diff", "--no-color", "--no-ext-diff"];
    if staged {
        args.push("--cached");
    }
    args.push("--");
    args.push(path);
    let out = git(sandbox.base(), &args).await?;

    let mut hunks: Vec<Hunk> = Vec::new();
    for line in out.lines() {
        if line.starts_with("@@") {
            hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            // Everything before the first `@@` is the file header git writes
            // (`diff --git`, `index`, `---`, `+++`), which the page already
            // knows: it asked about this path.
            continue;
        };
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => ("add", &line[1..]),
            Some(b'-') => ("del", &line[1..]),
            Some(b'\\') => continue, // "\ No newline at end of file"
            _ => ("ctx", line.get(1..).unwrap_or("")),
        };
        hunk.lines.push(DiffLine {
            kind,
            text: text.to_string(),
        });
    }

    let note = match hunks.is_empty() {
        true => Some(match staged {
            true => "Nothing staged for this file.".to_string(),
            false => {
                "No unstaged changes — the file may be untracked, or already staged.".to_string()
            }
        }),
        false => None,
    };

    Ok(Diff {
        path: path.to_string(),
        staged,
        hunks,
        note,
    })
}

/// Runs git in the workspace and hands back its stdout.
///
/// **Not through the tool sandbox**, and that is the one thing worth being
/// explicit about: this is the server reading its own working directory on
/// behalf of the person looking at it, not a model running a command. It is
/// also why the `/dev/null` and `~/.gitconfig` findings in
/// `RECORD/2026-09-15.git-could-not-open-dev-null.completed.md` do not reach
/// it — a child here inherits the server's own environment.
async fn git(base: &Path, args: &[&str]) -> Result<String, Error> {
    let run = Command::new("git")
        .arg("-C")
        .arg(base)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let out = tokio::time::timeout(GIT_TIMEOUT, run)
        .await
        .map_err(|_| Error::Failed("git did not answer in ten seconds".to_string()))?
        .map_err(|error| Error::Failed(format!("could not run git: {error}")))?;

    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        return Err(Error::Failed(match why.is_empty() {
            true => format!("git exited with {}", out.status),
            false => why.to_string(),
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---- following the disk ----------------------------------------------------

/// A batch closes once the disk has been quiet this long — a debounce, not a
/// fixed window. A save is several events (a temp file, a rename, a chmod), and
/// `cargo build` is thousands; the page wants one answer for each of those, not
/// one per event. A fixed 150 ms window after the first event answered a
/// seven-second build nineteen times.
const QUIET: std::time::Duration = std::time::Duration::from_millis(300);

/// And closes at this age whatever is still arriving, so a two-minute build
/// moves the tree every couple of seconds rather than only once it ends.
const MAX_BATCH: std::time::Duration = std::time::Duration::from_secs(2);

/// More paths than this in one batch and the page is told to re-read what it
/// has open rather than handed the list: past it, naming each one costs more
/// than re-reading.
const MAX_NAMED: usize = 1000;

/// One batch of what changed under the base.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Changes {
    /// Relative to the base, `/`-separated, sorted. Created, written, removed
    /// or renamed, without saying which: the page re-reads either way, and
    /// the listing it re-reads is what says which.
    pub paths: Vec<String>,
    /// Git's own state moved — the index, `HEAD` or a ref — with nothing in
    /// the working tree to say so. A `git add` in a terminal is the case.
    pub git: bool,
    /// Too many to name: re-read everything that is open.
    pub all: bool,
}

/// The base, watched, for as long as this value lives.
///
/// **One per stream, and dropped with it**: the watcher is owned here, so a
/// page that closes its tab stops the platform watching on its behalf. Nothing
/// is shared between two pages, because two pages are rare and a shared
/// watcher would need a reference count to know when nobody is listening.
///
/// **What it reports is bounded by the sandbox the way a listing is**: a
/// path is named only when its directory is one the panels may list, and a
/// listing already names everything in a directory it may read. So a change
/// says nothing the tree would not.
pub struct Watch {
    // Held only so it keeps watching; dropping it is what stops it.
    _watcher: notify::RecommendedWatcher,
    events: tokio::sync::mpsc::UnboundedReceiver<PathBuf>,
    sandbox: std::sync::Arc<Sandbox>,
    /// The base as the platform spells it. FSEvents reports canonical paths,
    /// and a base reached through a symlink (`/var` → `/private/var` on
    /// macOS) would strip nothing off them.
    canonical: PathBuf,
}

impl Watch {
    pub fn start(sandbox: std::sync::Arc<Sandbox>) -> Result<Watch, Error> {
        use notify::Watcher;

        let base = sandbox.base().to_path_buf();
        let canonical = base.canonicalize().unwrap_or_else(|_| base.clone());
        let (send, events) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                // Reading a file is not changing it, and inotify reports both.
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for path in event.paths {
                    // The receiver is gone once the stream is: nothing to do.
                    let _ = send.send(path);
                }
            })
            .map_err(|error| Error::Failed(format!("cannot watch the workspace: {error}")))?;
        watcher
            .watch(&canonical, notify::RecursiveMode::Recursive)
            .map_err(|error| Error::Failed(format!("cannot watch the workspace: {error}")))?;
        Ok(Watch {
            _watcher: watcher,
            events,
            sandbox,
            canonical,
        })
    }

    /// The next batch: waits for an event, then gathers until [`QUIET`] passes
    /// with none, or the batch is [`MAX_BATCH`] old. `None` once the watcher
    /// has stopped. A batch whose every path was left out is not answered — it
    /// waits for the next one instead.
    pub async fn next(&mut self) -> Option<Changes> {
        loop {
            let first = self.events.recv().await?;
            let mut raw = vec![first];
            let cap = tokio::time::sleep(MAX_BATCH);
            tokio::pin!(cap);
            loop {
                tokio::select! {
                    more = self.events.recv() => match more {
                        Some(path) => raw.push(path),
                        None => break,
                    },
                    _ = tokio::time::sleep(QUIET) => break,
                    _ = &mut cap => break,
                }
            }
            let changes = classify(&self.sandbox, &self.canonical, raw);
            if changes != Changes::default() {
                return Some(changes);
            }
        }
    }
}

/// What a batch of absolute paths means to the panels.
fn classify(sandbox: &Sandbox, canonical: &Path, raw: Vec<PathBuf>) -> Changes {
    let mut changes = Changes::default();
    let mut named = std::collections::BTreeSet::new();
    // Asked once per directory per batch: a build writes hundreds of files
    // into the same few, and each answer resolves symlinks on disk.
    let mut listable: std::collections::HashMap<PathBuf, bool> = std::collections::HashMap::new();
    for path in raw {
        let Ok(relative) = path.strip_prefix(canonical) else {
            continue;
        };
        let mut parts = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy());
        if parts.next().as_deref() == Some(".git") {
            // Only what moves `git status`. Objects, logs and lock files are
            // churn, and `git status` itself rewrites the index now and then —
            // which is why the page throttles what it does with this flag.
            let next = parts.next();
            changes.git |= matches!(next.as_deref(), Some("index" | "HEAD" | "refs"));
            continue;
        }
        // Equal to the base: nothing to name.
        let Some(parent) = path.parent().filter(|_| !relative.as_os_str().is_empty()) else {
            continue;
        };
        let allowed = *listable
            .entry(parent.to_path_buf())
            .or_insert_with(|| sandbox.check_path(parent, Access::Read).verdict.allowed);
        if allowed {
            named.insert(relative_of(canonical, &path));
        }
    }
    match named.len() > MAX_NAMED {
        true => changes.all = true,
        false => changes.paths = named.into_iter().collect(),
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain of directories that each hold one directory is followed to its
    /// end, and stops at the first that holds a file or a second entry.
    #[tokio::test]
    async fn a_directory_that_only_holds_a_directory_names_the_chain() {
        let root = std::env::temp_dir().join(format!("luu-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b/c/d")).unwrap();
        std::fs::write(root.join("a/b/c/d/file.rs"), "").unwrap();
        std::fs::write(root.join("a/b/c/stop.txt"), "").unwrap();
        std::fs::create_dir_all(root.join("two/x")).unwrap();
        std::fs::create_dir_all(root.join("two/y")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::create_dir_all(root.join("git/.git")).unwrap();
        std::fs::create_dir_all(root.join("git/inner")).unwrap();

        // `c` holds a file beside `d`, so the chain from `a` ends at `c`.
        assert_eq!(only_chain(&root.join("a")).await.as_deref(), Some("b/c"));
        assert_eq!(only_chain(&root.join("two")).await, None);
        assert_eq!(only_chain(&root.join("empty")).await, None);
        // `.git` is not content, here as in the listing.
        assert_eq!(
            only_chain(&root.join("git")).await.as_deref(),
            Some("inner")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory with a `.git` in it says so, whether `.git` is a directory
    /// or the file a worktree leaves; a plain one and a file do not.
    #[tokio::test]
    async fn a_folder_with_its_own_git_is_marked_a_repository() {
        let root = std::env::temp_dir().join(format!("luu-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cloned/.git")).unwrap();
        std::fs::create_dir_all(root.join("worktree")).unwrap();
        std::fs::write(root.join("worktree/.git"), "gitdir: elsewhere\n").unwrap();
        std::fs::create_dir_all(root.join("plain")).unwrap();
        std::fs::write(root.join("file.txt"), "").unwrap();
        let mut policy = agent_core::sandbox::SandboxPolicy::default();
        policy.allow(&root, Access::Read);
        let sandbox = Sandbox::new(&policy, &root).unwrap();

        let listed = tree(&sandbox, "").await.unwrap();
        let repo = |name: &str| listed.entries.iter().find(|e| e.name == name).unwrap().repo;
        assert!(repo("cloned"));
        assert!(repo("worktree"));
        assert!(!repo("plain"));
        assert!(!repo("file.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The count is the file's, not the payload's, and the two agree exactly
    /// when nothing was cut. Written because the off-by-one is the whole risk:
    /// the text is split on `\n`, which yields one more piece than there are
    /// newlines, so a count of newlines alone would be short by one on every
    /// file in the repository.
    #[test]
    fn the_line_count_is_the_one_the_lines_themselves_come_to() {
        for text in [
            "",
            "one line, no newline",
            "two\nlines\n",
            "trailing blank\n\n",
            "\n\n\n",
        ] {
            let counted = text.bytes().filter(|byte| *byte == b'\n').count() + 1;
            let (_, lines) = crate::highlight::lines("x.txt", text);
            assert_eq!(
                counted,
                lines.len(),
                "{text:?} counts {counted} and splits into {}",
                lines.len()
            );
        }
    }

    /// The rename record eats its second field. Written because the loop that
    /// does it is the one piece of parsing here that a reader cannot check by
    /// eye: `-z` output has no line breaks to count.
    #[test]
    fn a_rename_does_not_leave_its_old_name_as_a_record() {
        // `R  new\0old\0 M other\0` — one rename and one modification.
        let raw = "R  new\0old\0 M other\0";
        let mut map = BTreeMap::new();
        let mut fields = raw.split('\0').filter(|part| !part.is_empty());
        while let Some(record) = fields.next() {
            let (code, path) = match record.len() > 3 {
                true => (record[..2].to_string(), record[3..].to_string()),
                false => continue,
            };
            if code.starts_with('R') || code.starts_with('C') {
                let _ = fields.next();
            }
            map.insert(path, code);
        }
        // Both positions kept: "R " is a rename staged in the index, where
        // " R" would be one in the working tree, and the space is the only
        // thing that says which.
        assert_eq!(map.get("new").map(String::as_str), Some("R "));
        assert_eq!(map.get("other").map(String::as_str), Some(" M"));
        assert!(
            !map.contains_key("old"),
            "the old name is not its own entry"
        );
    }
}
