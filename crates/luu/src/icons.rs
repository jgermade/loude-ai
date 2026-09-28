//! A VSCode icon theme, loaded from wherever it is installed on this machine.
//!
//! **Nothing is vendored.** The alternative was shipping one theme's art in the
//! binary — measured at 1.5 MB and 263 files for `vscode-great-icons` — which
//! would tie the page to one person's taste and make this repository a
//! redistributor. The pattern used instead is the one `luu.toml` already uses
//! for `~/.cargo`: the theme lives on your machine, and you name it.
//!
//! ```toml
//! [ui]
//! icon-theme = "~/.vscode/extensions/emmanuelbeziat.vscode-great-icons-3.0.0"
//! ```
//!
//! Either an extension directory (whose `package.json` declares
//! `contributes.iconThemes`) or a theme JSON directly.
//!
//! **The id is the security design.** No path from the client ever reaches the
//! filesystem: the theme is read once into an id → absolute path table, and
//! only ids in that table can be served. There is no traversal to get wrong
//! because there is no path to traverse. See
//! `RECORD/2026-09-15.a-three-pane-inspector.completed.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What the page needs to pick an icon, by id.
///
/// The maps are the theme's own, passed through: matching a filename against
/// them is the page's job, and doing it here would mean a round trip per row.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Manifest {
    /// Absent when no theme is configured, so the page knows to draw its own
    /// two glyphs rather than to wait for icons that are not coming.
    pub loaded: bool,
    /// The theme's name, for the preferences panel to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Moves every time the server swaps the theme, and the page puts it in
    /// each icon's URL. Icons are served cacheable, so without it a new theme
    /// would draw the old one's art until the cache let go.
    pub revision: u64,
    pub file_extensions: HashMap<String, String>,
    pub file_names: HashMap<String, String>,
    pub folder_names: HashMap<String, String>,
    /// The fallbacks the theme declares, when it declares them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_expanded: Option<String>,
}

/// A loaded theme: what the page is told, and where each id's bytes are.
#[derive(Debug, Clone, Default)]
pub struct Theme {
    pub manifest: Manifest,
    /// Id → the file on disk. The only paths this surface will ever open.
    paths: HashMap<String, PathBuf>,
}

impl Theme {
    /// The file for an id, or `None` for an id this theme never declared.
    pub fn path(&self, id: &str) -> Option<&Path> {
        self.paths.get(id).map(PathBuf::as_path)
    }
}

/// The subset of a VSCode icon theme this reads.
///
/// Everything else in the format — `light`, `highContrast`, `folderExpanded`
/// per folder, `hidesExplorerArrows` — is either a variant this page does not
/// render or a hint it does not need, and `serde` ignores what is not named.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThemeFile {
    #[serde(default)]
    icon_definitions: HashMap<String, Definition>,
    #[serde(default)]
    file_extensions: HashMap<String, String>,
    #[serde(default)]
    file_names: HashMap<String, String>,
    #[serde(default)]
    folder_names: HashMap<String, String>,
    file: Option<String>,
    folder: Option<String>,
    folder_expanded: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Definition {
    icon_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionManifest {
    #[serde(default)]
    contributes: Contributes,
    display_name: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Contributes {
    #[serde(default)]
    icon_themes: Vec<IconThemeEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IconThemeEntry {
    path: String,
    label: Option<String>,
}

/// Finds the theme JSON, given what the person wrote in `config.toml`.
///
/// A directory is an installed extension: its `package.json` says which of its
/// files is the theme, and a theme that declares several gets the first, which
/// is the one VSCode shows first too.
fn locate(named: &Path) -> Result<(PathBuf, Option<String>), String> {
    if named.is_file() {
        return Ok((named.to_path_buf(), None));
    }
    // An extension's directory carries its version, and the editor replaces
    // it on every update. The path that was written stays what it was; what
    // it resolves to follows the extension.
    let updated;
    let named = match named.is_dir() {
        true => named,
        false => match newest_sibling(named) {
            Some(found) => {
                updated = found;
                &updated
            }
            None => return Err(format!("{}: no such file or directory", named.display())),
        },
    };
    let package = named.join("package.json");
    // A folder with no `package.json` is a theme somebody copied out of one,
    // or imported without its manifest: its theme is the JSON at its top that
    // has `iconDefinitions`.
    if !package.is_file() {
        return theme_json_in(named)
            .map(|path| (path, None))
            .ok_or_else(|| {
                format!(
                    "{}: has neither a package.json nor a theme JSON (one with iconDefinitions)",
                    named.display()
                )
            });
    }
    let raw = std::fs::read_to_string(&package)
        .map_err(|error| format!("{}: {error}", package.display()))?;
    let manifest: ExtensionManifest =
        serde_json::from_str(&raw).map_err(|error| format!("{}: {error}", package.display()))?;
    let entry = manifest
        .contributes
        .icon_themes
        .into_iter()
        .next()
        .ok_or_else(|| {
            format!(
                "{}: declares no icon theme (contributes.iconThemes is empty)",
                package.display()
            )
        })?;
    let label = entry.label.or(manifest.display_name).or(manifest.name);
    Ok((named.join(entry.path), label))
}

/// Reads a theme, resolving every icon path against the theme file's own
/// directory.
///
/// A definition whose file is missing is dropped rather than failing the load:
/// themes carry entries for icons they no longer ship, and one stale line
/// should not cost a person every icon.
pub fn load(named: &Path) -> Result<Theme, String> {
    let (theme_path, label) = locate(named)?;
    let base = theme_path
        .parent()
        .ok_or_else(|| format!("{}: has no directory", theme_path.display()))?
        .to_path_buf();
    let raw = std::fs::read_to_string(&theme_path)
        .map_err(|error| format!("{}: {error}", theme_path.display()))?;
    // VSCode's own themes are JSON with comments often enough that a strict
    // parser is the wrong tool; `serde_json` is strict, so a theme that uses
    // them fails with a message naming the line, which is a better answer than
    // silently half-loading.
    let theme: ThemeFile =
        serde_json::from_str(&raw).map_err(|error| format!("{}: {error}", theme_path.display()))?;

    let mut paths = HashMap::new();
    for (id, definition) in &theme.icon_definitions {
        let Some(icon_path) = &definition.icon_path else {
            continue;
        };
        let resolved = base.join(icon_path.trim_start_matches("./"));
        // Canonicalized, and that is what makes the table trustworthy: an
        // `iconPath` of `../../../etc/shadow` resolves to a real path here and
        // is then refused for being outside the theme's own directory.
        let Ok(resolved) = resolved.canonicalize() else {
            continue;
        };
        let Ok(base_real) = base.canonicalize() else {
            continue;
        };
        if !resolved.starts_with(&base_real) {
            continue;
        }
        paths.insert(id.clone(), resolved);
    }

    // A map entry pointing at an id with no file is dropped too, so the page
    // never asks for an icon that cannot answer.
    let keep = |map: HashMap<String, String>| -> HashMap<String, String> {
        map.into_iter()
            .filter(|(_, id)| paths.contains_key(id))
            .collect()
    };
    let keep_one = |id: Option<String>| id.filter(|id| paths.contains_key(id));

    Ok(Theme {
        manifest: Manifest {
            loaded: true,
            name: label,
            // The server's to set, when it swaps this in.
            revision: 0,
            file_extensions: keep(theme.file_extensions),
            file_names: keep(theme.file_names),
            folder_names: keep(theme.folder_names),
            file: keep_one(theme.file),
            folder: keep_one(theme.folder),
            folder_expanded: keep_one(theme.folder_expanded),
        },
        paths,
    })
}

/// `publisher.name-1.2.3` split into its prefix and its version, when the last
/// component of a path looks like that.
fn versioned(name: &str) -> Option<(&str, Vec<u64>)> {
    let (prefix, version) = name.rsplit_once('-')?;
    if !version.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let numbers = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    Some((prefix, numbers))
}

/// The newest `publisher.name-<version>` beside a versioned path that is gone.
fn newest_sibling(named: &Path) -> Option<PathBuf> {
    let (prefix, _) = versioned(named.file_name()?.to_str()?)?;
    std::fs::read_dir(named.parent()?)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let (other, version) = versioned(&name)?;
            (other == prefix).then(|| (version, entry.path()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, path)| path)
}

/// The theme JSON at the top of a folder: the first, by name, that declares
/// `iconDefinitions`.
fn theme_json_in(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    candidates.sort();
    candidates.into_iter().find(|path| {
        std::fs::read_to_string(path).is_ok_and(|raw| raw.contains("\"iconDefinitions\""))
    })
}

/// A theme this machine has, for the preferences panel to offer.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Found {
    /// As it would be written into `config.toml`: `~/…` when it is under the
    /// home directory, so the file reads the same on every machine that has
    /// the theme in the same place.
    pub path: String,
    pub name: String,
    /// Where it was found: `luu` for an import, or the editor's name.
    pub source: &'static str,
}

/// The editors whose extension directories are looked in, relative to the
/// home directory.
const EDITORS: &[(&str, &str)] = &[
    (".vscode/extensions", "VS Code"),
    (".vscode-insiders/extensions", "VS Code Insiders"),
    (".vscode-oss/extensions", "VSCodium"),
    (".cursor/extensions", "Cursor"),
    (".windsurf/extensions", "Windsurf"),
];

/// Every icon theme on this machine: the imports first, then each editor's
/// installed extensions that declare one, newest version only.
///
/// Only a directory's own children are read, and only their `package.json`
/// (or, for an import, the theme JSON), so this is cheap enough to run each
/// time the panel opens.
pub fn discover(imported: Option<&Path>, home: Option<&Path>) -> Vec<Found> {
    let mut found = Vec::new();
    if let Some(dir) = imported {
        let mut here: Vec<Found> = children(dir)
            .filter(|path| !hidden(path))
            .filter_map(|path| {
                let (_, label) = locate(&path).ok()?;
                let name = label.unwrap_or_else(|| file_name(&path));
                Some(Found {
                    path: tilde(&path, home),
                    name,
                    source: "luu",
                })
            })
            .collect();
        here.sort_by(|a, b| a.name.cmp(&b.name));
        found.extend(here);
    }
    let Some(home) = home else {
        return found;
    };
    for (relative, source) in EDITORS {
        // Newest version of each extension: an editor keeps the old directory
        // around for a while after an update.
        let mut newest: HashMap<String, (Vec<u64>, PathBuf, String)> = HashMap::new();
        for path in children(&home.join(relative)) {
            let Some(label) = declared_theme(&path) else {
                continue;
            };
            let name = file_name(&path);
            let (prefix, version) = versioned(&name)
                .map(|(prefix, version)| (prefix.to_string(), version))
                .unwrap_or_else(|| (name.clone(), Vec::new()));
            match newest.get(&prefix) {
                Some((kept, _, _)) if *kept >= version => {}
                _ => {
                    newest.insert(prefix, (version, path, label));
                }
            }
        }
        let mut here: Vec<Found> = newest
            .into_values()
            .map(|(_, path, name)| Found {
                path: tilde(&path, Some(home)),
                name,
                source,
            })
            .collect();
        here.sort_by(|a, b| a.name.cmp(&b.name));
        found.extend(here);
    }
    found
}

/// The label of the icon theme an extension declares, if it declares one.
fn declared_theme(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let manifest: ExtensionManifest = serde_json::from_str(&raw).ok()?;
    let entry = manifest.contributes.icon_themes.into_iter().next()?;
    Some(
        entry
            .label
            .or(manifest.display_name)
            .or(manifest.name)
            .unwrap_or_else(|| file_name(dir)),
    )
}

fn children(dir: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
}

fn hidden(path: &Path) -> bool {
    file_name(path).starts_with('.')
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The most an import may write. Great Icons is 2 MB in 317 files.
pub const IMPORT_MAX_BYTES: usize = 32 * 1024 * 1024;
pub const IMPORT_MAX_FILES: usize = 5_000;

/// Writes a picked folder under `root` and returns where it landed, **only if
/// it loads as a theme**.
///
/// `files` are `(relative path, bytes)` as a browser's directory picker sends
/// them, every path starting with the picked folder's own name. That name,
/// reduced to `[A-Za-z0-9._-]`, is what the import is called.
///
/// Every path is re-checked here, because it came off the network: only
/// normal components, so no `..`, no root and no drive. The folder is written
/// to a hidden sibling first and renamed into place once it loads, so a bad
/// import never replaces a good one of the same name.
pub fn import(root: &Path, files: Vec<(String, Vec<u8>)>) -> Result<PathBuf, String> {
    if files.is_empty() {
        return Err("the folder was empty".into());
    }
    if files.len() > IMPORT_MAX_FILES {
        return Err(format!("more than {IMPORT_MAX_FILES} files"));
    }
    let total: usize = files.iter().map(|(_, bytes)| bytes.len()).sum();
    if total > IMPORT_MAX_BYTES {
        return Err(format!("more than {} MiB", IMPORT_MAX_BYTES / 1024 / 1024));
    }

    let mut checked = Vec::with_capacity(files.len());
    let mut top: Option<String> = None;
    for (relative, bytes) in files {
        let relative = relative.replace('\\', "/");
        let path = Path::new(&relative);
        let normal = path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
        if !normal || relative.is_empty() {
            return Err(format!("{relative}: not a path inside the folder"));
        }
        let mut parts = path.components();
        let first = parts
            .next()
            .map(|part| part.as_os_str().to_string_lossy().into_owned());
        let rest: PathBuf = parts.collect();
        if rest.as_os_str().is_empty() {
            return Err(format!("{relative}: not inside a picked folder"));
        }
        match (&top, first) {
            (None, Some(first)) => top = Some(first),
            (Some(kept), Some(first)) if *kept == first => {}
            _ => return Err("the files are not from one folder".into()),
        }
        checked.push((rest, bytes));
    }

    let name: String = top
        .unwrap_or_default()
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                true => c,
                false => '-',
            },
        )
        .collect::<String>()
        .trim_start_matches('.')
        .to_string();
    if name.is_empty() {
        return Err("the folder has no usable name".into());
    }

    std::fs::create_dir_all(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let staging = root.join(format!(".{name}.importing"));
    let _ = std::fs::remove_dir_all(&staging);
    let written = (|| -> Result<(), String> {
        for (rest, bytes) in &checked {
            let target = staging.join(rest);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            std::fs::write(&target, bytes)
                .map_err(|error| format!("{}: {error}", target.display()))?;
        }
        load(&staging).map(|_| ())
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    let target = root.join(&name);
    if target.exists() {
        std::fs::remove_dir_all(&target)
            .map_err(|error| format!("{}: {error}", target.display()))?;
    }
    std::fs::rename(&staging, &target).map_err(|error| format!("{}: {error}", target.display()))?;
    Ok(target)
}

/// The content type for an icon, by extension. Small and closed: these are the
/// three things an icon theme ships, and guessing beyond them would mean
/// serving whatever a theme put in its directory as whatever it claimed.
pub fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn theme_dir() -> (tempdir::Dir, PathBuf) {
        let dir = tempdir::Dir::new("icons");
        let icons = dir.path().join("icons");
        fs::create_dir_all(&icons).unwrap();
        fs::write(icons.join("rust.svg"), "<svg/>").unwrap();
        fs::write(icons.join("file.svg"), "<svg/>").unwrap();
        fs::write(
            dir.path().join("theme.json"),
            r#"{
              "iconDefinitions": {
                "_rust": { "iconPath": "./icons/rust.svg" },
                "_file": { "iconPath": "./icons/file.svg" },
                "_gone": { "iconPath": "./icons/missing.svg" },
                "_escape": { "iconPath": "../../../etc/hosts" }
              },
              "fileExtensions": { "rs": "_rust", "zzz": "_gone" },
              "fileNames": { "Cargo.toml": "_rust" },
              "file": "_file"
            }"#,
        )
        .unwrap();
        let path = dir.path().join("theme.json");
        (dir, path)
    }

    #[test]
    fn a_theme_json_loads_and_keeps_only_icons_that_exist() {
        let (_dir, path) = theme_dir();
        let theme = load(&path).expect("theme loads");
        assert!(theme.manifest.loaded);
        assert_eq!(theme.manifest.file_extensions.get("rs").unwrap(), "_rust");
        // The definition whose file is missing is dropped, and so is the map
        // entry that pointed at it — the page never asks for a dead id.
        assert!(!theme.manifest.file_extensions.contains_key("zzz"));
        assert!(theme.path("_gone").is_none());
    }

    /// The one that matters: an `iconPath` that climbs out of the theme's own
    /// directory is not servable, however real the file it names is.
    #[test]
    fn an_icon_path_outside_the_theme_is_refused() {
        let (_dir, path) = theme_dir();
        let theme = load(&path).expect("theme loads");
        assert!(
            theme.path("_escape").is_none(),
            "a theme must not be able to hand out /etc/hosts"
        );
    }

    #[test]
    fn an_extension_directory_is_read_through_its_package_json() {
        let (dir, _) = theme_dir();
        fs::write(
            dir.path().join("package.json"),
            r#"{
              "name": "great-icons",
              "displayName": "Great Icons",
              "contributes": { "iconThemes": [{ "id": "g", "label": "Great", "path": "./theme.json" }] }
            }"#,
        )
        .unwrap();
        let theme = load(dir.path()).expect("extension loads");
        assert_eq!(theme.manifest.name.as_deref(), Some("Great"));
        assert!(theme.path("_rust").is_some());
    }

    #[test]
    fn a_path_that_is_not_there_says_so() {
        let error = load(Path::new("/nonexistent/theme")).unwrap_err();
        assert!(error.contains("no such file"), "{error}");
    }

    /// An extension directory as an editor lays it out, under `root`.
    fn extension(root: &Path, dir: &str, label: &str) -> PathBuf {
        let path = root.join(dir);
        fs::create_dir_all(path.join("icons")).unwrap();
        fs::write(path.join("icons/rust.svg"), "<svg/>").unwrap();
        fs::write(
            path.join("theme.json"),
            r#"{ "iconDefinitions": { "_rust": { "iconPath": "./icons/rust.svg" } },
                 "fileExtensions": { "rs": "_rust" } }"#,
        )
        .unwrap();
        fs::write(
            path.join("package.json"),
            format!(
                r#"{{ "name": "x", "contributes": {{ "iconThemes": [{{ "id": "x", "label": "{label}", "path": "./theme.json" }}] }} }}"#
            ),
        )
        .unwrap();
        path
    }

    /// The path written before the editor updated the extension keeps working.
    #[test]
    fn a_versioned_path_follows_the_extension_to_its_newest_version() {
        let root = tempdir::Dir::new("icons-versioned");
        extension(root.path(), "pub.great-icons-2.1.9", "Old");
        extension(root.path(), "pub.great-icons-2.1.121", "New");
        let written = root.path().join("pub.great-icons-2.0.0");
        let theme = load(&written).expect("a newer sibling is found");
        assert_eq!(theme.manifest.name.as_deref(), Some("New"));
        // And an unrelated name is still an error, not a guess.
        assert!(load(&root.path().join("pub.other-1.0.0")).is_err());
    }

    /// A folder copied out of an extension without its `package.json` is found
    /// by its theme JSON.
    #[test]
    fn a_folder_without_a_package_json_is_read_through_its_theme_json() {
        let (dir, _) = theme_dir();
        let theme = load(dir.path()).expect("the theme JSON is found");
        assert!(theme.path("_rust").is_some());
    }

    #[test]
    fn discover_finds_imports_and_the_newest_editor_extension() {
        let home = tempdir::Dir::new("icons-home");
        let editors = home.path().join(".vscode/extensions");
        extension(&editors, "pub.great-icons-2.1.9", "Great old");
        extension(&editors, "pub.great-icons-2.1.121", "Great");
        fs::create_dir_all(editors.join("pub.not-a-theme-1.0.0")).unwrap();
        let imported = home.path().join(".config/luu/icon-themes");
        extension(&imported, "mine", "Mine");

        let found = discover(Some(&imported), Some(home.path()));
        let names: Vec<_> = found.iter().map(|f| (f.name.as_str(), f.source)).collect();
        assert_eq!(names, vec![("Mine", "luu"), ("Great", "VS Code")]);
        assert_eq!(
            found[1].path,
            "~/.vscode/extensions/pub.great-icons-2.1.121"
        );
    }

    fn picked(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        for entry in ["package.json", "theme.json", "icons/rust.svg"] {
            files.push((format!("great/{entry}"), fs::read(dir.join(entry)).unwrap()));
        }
        files
    }

    #[test]
    fn an_import_lands_under_its_folder_name_and_loads() {
        let source = tempdir::Dir::new("icons-source");
        let dir = extension(source.path(), "great", "Great");
        let root = tempdir::Dir::new("icons-root");
        let landed = import(root.path(), picked(&dir)).expect("imports");
        assert_eq!(landed, root.path().join("great"));
        assert_eq!(
            load(&landed).unwrap().manifest.name.as_deref(),
            Some("Great")
        );
    }

    /// Paths off the network are re-checked: nothing may climb out.
    #[test]
    fn an_import_refuses_a_path_that_leaves_the_folder() {
        let root = tempdir::Dir::new("icons-escape");
        for bad in ["great/../../evil.svg", "/etc/passwd", "loose.svg"] {
            let files = vec![(bad.to_string(), b"x".to_vec())];
            assert!(import(root.path(), files).is_err(), "{bad} was accepted");
        }
        assert!(!root.path().join("evil.svg").exists());
    }

    /// A folder that does not load never replaces one that does.
    #[test]
    fn a_bad_import_leaves_the_good_one_in_place() {
        let source = tempdir::Dir::new("icons-good");
        let dir = extension(source.path(), "great", "Great");
        let root = tempdir::Dir::new("icons-keep");
        import(root.path(), picked(&dir)).unwrap();
        let broken = vec![("great/readme.md".to_string(), b"not a theme".to_vec())];
        assert!(import(root.path(), broken).is_err());
        assert!(load(&root.path().join("great")).is_ok());
        assert!(!root.path().join(".great.importing").exists());
    }

    /// A tiny scratch directory, removed on drop. Beside the tests that use it
    /// rather than pulled in as a dependency: this is the only place in the
    /// crate that wants one.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new(tag: &str) -> Self {
                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = std::env::temp_dir().join(format!("luu-{tag}-{unique}"));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
