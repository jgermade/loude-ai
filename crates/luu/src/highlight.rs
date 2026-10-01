//! Syntax highlighting for the content viewer, resolved here and never in the
//! browser.
//!
//! The same call `luu-design.md` makes for prompt diffs — "compute it in Rust
//! and send resolved spans" — and for the same two reasons: a highlighter in
//! the page would be the bundler this project removed on purpose, and the
//! server is already holding the bytes.
//!
//! **The wire format is pre-sliced chunks, not offsets.** A span carried as
//! `{start, end}` is a Rust *byte* index that JavaScript would read as a
//! UTF-16 index, which agrees for ASCII and quietly stops agreeing at the
//! first accented character in a comment. Sending the text already cut removes
//! the question. See `RECORD/2026-09-15.a-three-pane-inspector.completed.md`.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Serialize;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// One run of same-coloured text. `kind` is `None` for text no capture
/// matched, which is most of a file.
#[derive(Debug, Clone, Serialize)]
pub struct Chunk {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
}

/// The capture names asked for, and the class names the page styles.
///
/// Deliberately short. Tree-sitter's ecosystem has a hundred-odd capture names
/// and a viewer that colours all of them is a viewer nobody can read; these are
/// the distinctions that survive being looked at in a side panel. A query
/// capture that is not listed here is simply not captured, which is why the
/// list is also the allowlist.
///
/// The order matters only in that it is the index space
/// `tree_sitter_highlight` hands back.
const NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constructor",
    "function",
    "keyword",
    "number",
    "operator",
    "property",
    "punctuation",
    "string",
    "tag",
    "type",
    "variable",
    // Markdown's. Its grammar names prose rather than code, so none of the
    // fourteen above reach it: a README came back as punctuation and nothing
    // else.
    "text.title",
    "text.literal",
    "text.uri",
    "text.reference",
    "text.emphasis",
    "text.strong",
];

/// What the page gets as a CSS class, per index into [`NAMES`]. One to one
/// today; a separate table because the two are separate decisions — a capture
/// name is tree-sitter's, a class name is this page's.
fn class_of(index: usize) -> Option<&'static str> {
    // A class is one word, so a dotted capture gets a name of its own. A label
    // and a destination are both a link to somebody reading the file.
    Some(match NAMES.get(index).copied()? {
        "text.title" => "heading",
        "text.literal" => "literal",
        "text.uri" | "text.reference" => "link",
        "text.emphasis" => "emphasis",
        "text.strong" => "strong",
        name => name,
    })
}

/// Which grammar a path gets, by extension and then by filename.
///
/// One table, so a language is one line. Returns the language's name as the
/// page shows it, alongside the grammar and its queries.
/// The grammar for a path, by filename first and extension second.
///
/// Separate from the table below on purpose: this half is a fact about
/// filenames and changes when somebody adds an extension, that half is a fact
/// about crates and changes when somebody adds a grammar. A name with no entry
/// in the table simply has no grammar, and the file is served as text — which
/// is what `Dockerfile` was, and `Makefile` was not even asked about.
fn language_of(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");

    // Filenames first: `Makefile` and `Dockerfile` have no extension, and
    // `.gitignore`'s "extension" is the whole name.
    let key = match name {
        "Cargo.lock" => "toml",
        "Dockerfile" | "Containerfile" => "dockerfile",
        "Makefile" | "makefile" | "GNUmakefile" => "make",
        _ => ext,
    };

    Some(match key {
        "rs" => "rust",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "html" | "htm" => "html",
        "css" => "css",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "json" => "json",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" | "zsh" => "bash",
        "dockerfile" | "containerfile" => "dockerfile",
        "mk" | "make" => "make",
        _ => return None,
    })
}

/// Every grammar, compiled once for the life of the process.
///
/// **This used to be built per request**, and that was most of what opening a
/// file cost: `HighlightConfiguration::new` compiles the grammar's queries,
/// measured at ~15 ms for Rust against ~25 ms to parse the 153 KB file it was
/// being rebuilt for — so thirteen bytes of Rust cost 15 ms and 153 KB cost 40.
/// A configuration is immutable once `configure` has run, so one per language
/// is all there ever needs to be. See the phase 8 section of
/// `RECORD/2026-09-15.a-three-pane-inspector.completed.md`.
///
/// A grammar whose queries will not compile is dropped rather than panicking,
/// which is what the `.ok()` was doing before: one bad grammar should not take
/// the other twelve, and the file it was for is still readable as text.
static GRAMMARS: LazyLock<HashMap<&'static str, HighlightConfiguration>> = LazyLock::new(|| {
    let mut grammars = HashMap::new();
    let mut add = |name: &'static str,
                   language: tree_sitter::Language,
                   highlights: &str,
                   injections: &str,
                   locals: &str| {
        if let Ok(mut config) =
            HighlightConfiguration::new(language, name, highlights, injections, locals)
        {
            config.configure(NAMES);
            grammars.insert(name, config);
        }
    };

    add(
        "rust",
        tree_sitter_rust::LANGUAGE.into(),
        tree_sitter_rust::HIGHLIGHTS_QUERY,
        tree_sitter_rust::INJECTIONS_QUERY,
        "",
    );
    add(
        "javascript",
        tree_sitter_javascript::LANGUAGE.into(),
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::INJECTIONS_QUERY,
        tree_sitter_javascript::LOCALS_QUERY,
    );
    // TypeScript's query holds only what TypeScript adds, and inherits the
    // rest from JavaScript's, the way the grammar does. Alone it found two
    // keywords in a whole `.ts` file. Its own patterns go first so that they
    // win where the two disagree, and TSX also takes JavaScript's JSX half.
    let typescript = format!(
        "{}\n{}",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        tree_sitter_javascript::HIGHLIGHT_QUERY,
    );
    let tsx = format!(
        "{typescript}\n{}",
        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
    );
    add(
        "typescript",
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        &typescript,
        tree_sitter_javascript::INJECTIONS_QUERY,
        tree_sitter_typescript::LOCALS_QUERY,
    );
    add(
        "tsx",
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        &tsx,
        tree_sitter_javascript::INJECTIONS_QUERY,
        tree_sitter_typescript::LOCALS_QUERY,
    );
    add(
        "html",
        tree_sitter_html::LANGUAGE.into(),
        tree_sitter_html::HIGHLIGHTS_QUERY,
        tree_sitter_html::INJECTIONS_QUERY,
        "",
    );
    add(
        "css",
        tree_sitter_css::LANGUAGE.into(),
        tree_sitter_css::HIGHLIGHTS_QUERY,
        "",
        "",
    );
    add(
        "toml",
        tree_sitter_toml_ng::LANGUAGE.into(),
        tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        "",
        "",
    );
    add(
        "markdown",
        tree_sitter_md::LANGUAGE.into(),
        tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        tree_sitter_md::INJECTION_QUERY_BLOCK,
        "",
    );
    // Never chosen by a path, and never injected: `markdown` runs it over
    // every paragraph, heading and list item itself, because that is where
    // emphasis, code spans and links are.
    add(
        "markdown_inline",
        tree_sitter_md::INLINE_LANGUAGE.into(),
        tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
        tree_sitter_md::INJECTION_QUERY_INLINE,
        "",
    );
    add(
        "json",
        tree_sitter_json::LANGUAGE.into(),
        tree_sitter_json::HIGHLIGHTS_QUERY,
        "",
        "",
    );
    add(
        "python",
        tree_sitter_python::LANGUAGE.into(),
        tree_sitter_python::HIGHLIGHTS_QUERY,
        "",
        "",
    );
    add(
        "go",
        tree_sitter_go::LANGUAGE.into(),
        tree_sitter_go::HIGHLIGHTS_QUERY,
        "",
        "",
    );
    add(
        "c",
        tree_sitter_c::LANGUAGE.into(),
        tree_sitter_c::HIGHLIGHT_QUERY,
        "",
        "",
    );
    add(
        "cpp",
        tree_sitter_cpp::LANGUAGE.into(),
        tree_sitter_cpp::HIGHLIGHT_QUERY,
        "",
        "",
    );
    add(
        "yaml",
        tree_sitter_yaml::LANGUAGE.into(),
        tree_sitter_yaml::HIGHLIGHTS_QUERY,
        "",
        "",
    );
    add(
        "bash",
        tree_sitter_bash::LANGUAGE.into(),
        tree_sitter_bash::HIGHLIGHT_QUERY,
        "",
        "",
    );
    // Its `RUN` bodies and heredocs are handed to `bash`, `json`, `yaml` and
    // `toml` by the crate's own injections.
    add(
        "dockerfile",
        tree_sitter_containerfile::LANGUAGE.into(),
        tree_sitter_containerfile::HIGHLIGHTS_QUERY,
        tree_sitter_containerfile::INJECTIONS_QUERY,
        "",
    );
    // The crate's query speaks Neovim's older names — `@conditional`,
    // `@include`, `@repeat`, `@exception` — none of which [`NAMES`] lists, so
    // alone `ifeq` and `include` came back plain and a target was not told
    // apart from its prerequisites. These go *last*: where two patterns
    // capture the same node, this version of `tree_sitter_highlight` keeps the
    // later one — first, they lost to `@include` and `@conditional`.
    let make = format!(
        "{}\n{}",
        tree_sitter_make::HIGHLIGHTS_QUERY,
        r#"
[
 "ifeq" "ifneq" "ifdef" "ifndef" "else" "endif"
 "include" "sinclude" "-include"
 "if" "or" "and" "foreach" "error" "warning" "info"
] @keyword
(targets (word) @function)
"#,
    );
    add("make", tree_sitter_make::LANGUAGE.into(), &make, "", "");
    grammars
});

/// Compiles every grammar now, so that no request is the one that pays for it.
///
/// Called from `serve`'s startup beside the icon theme, and for the same
/// reason: a cost that is going to be paid once should be paid where somebody
/// is watching the process start, not inside the first click.
pub fn warm() {
    LazyLock::force(&GRAMMARS);
}

fn language_for(path: &str) -> Option<(&'static str, &'static HighlightConfiguration)> {
    let name = language_of(path)?;
    Some((name, GRAMMARS.get(name)?))
}

/// The grammar an injection asks for by name: a fenced block's info string
/// (```` ```rust ````, ```` ```sh ````), or a name a query sets itself
/// (`markdown_inline`, HTML's `javascript` and `css`).
///
/// A grammar's own name first, then the name read as an extension, so a fence
/// can say `rust` or `rs` and mean the same thing. An unknown one is left as
/// text, which is what a fence in a language nobody installed should be.
fn injected(name: &str) -> Option<&'static HighlightConfiguration> {
    // Asked for by the block grammar and run by `markdown` instead: as an
    // injection it parses and then highlights nothing, so running it here
    // would only be a second parse of every paragraph.
    if name.trim().eq_ignore_ascii_case("markdown_inline") {
        return None;
    }
    fence_language(name).map(|(_, config)| config)
}

/// A fence's info string, read the way [`injected`] reads it: the grammar's own
/// name, then the name as an extension, with the shell's other names. Named as
/// well as found, because a reply's snippet says which language it was taken
/// for.
fn fence_language(name: &str) -> Option<(&'static str, &'static HighlightConfiguration)> {
    let name = name.trim().to_ascii_lowercase();
    let name = match name.as_str() {
        "shell" | "console" => "bash",
        other => other,
    };
    GRAMMARS
        .get_key_value(name)
        .map(|(name, config)| (*name, config))
        .or_else(|| language_for(&format!("x.{name}")))
}

/// The cap above which a file is served unhighlighted.
///
/// Below [`crate::workspace::MAX_FILE_BYTES`] on purpose: a big file still
/// opens, it just opens as text. Parsing a half-megabyte of minified
/// JavaScript to colour it is a cost nobody asked for by clicking a filename.
pub const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;

/// One line per line of the file, each a list of chunks.
///
/// Every file comes back in this shape, highlighted or not: a language with no
/// grammar, a file too big to parse and a parse that failed all produce one
/// chunk per line with no kind, so the viewer has one code path instead of
/// three.
pub fn lines(path: &str, text: &str) -> (Option<&'static str>, Vec<Vec<Chunk>>) {
    run(language_for(path), text)
}

/// The same lines for a fenced block in a reply, whose language is the fence's
/// info string rather than a path: `rust`, `rs`, `sh`. An unknown or empty one
/// is plain text, as it is in a Markdown file.
pub fn fenced(language: &str, text: &str) -> (Option<&'static str>, Vec<Vec<Chunk>>) {
    run(fence_language(language), text)
}

fn run(
    language: Option<(&'static str, &'static HighlightConfiguration)>,
    text: &str,
) -> (Option<&'static str>, Vec<Vec<Chunk>>) {
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return (None, plain(text));
    }
    let Some((name, config)) = language else {
        return (None, plain(text));
    };
    let highlighted = match name {
        "markdown" => markdown(text),
        _ => highlighted(text, config),
    };
    match highlighted {
        Some(lines) => (Some(name), lines),
        // A grammar that failed on this file is not worth a message: the file
        // is still readable, which is what the panel is for.
        None => (None, plain(text)),
    }
}

fn plain(text: &str) -> Vec<Vec<Chunk>> {
    text.split('\n')
        .map(|line| match line.is_empty() {
            true => Vec::new(),
            false => vec![Chunk {
                text: line.to_string(),
                kind: None,
            }],
        })
        .collect()
}

/// One run of the source, as byte offsets, and the class it gets.
type Span = (usize, usize, Option<&'static str>);

/// Runs the highlighter and cuts its events into lines.
fn highlighted(text: &str, config: &HighlightConfiguration) -> Option<Vec<Vec<Chunk>>> {
    cut(text, &spans(text, config)?)
}

/// What the highlighter says about each run of `text`, innermost capture
/// winning.
fn spans(text: &str, config: &HighlightConfiguration) -> Option<Vec<Span>> {
    let mut highlighter = Highlighter::new();
    // `None` twice: no encoding override, and no cancellation flag — the
    // 256 KB cap above is what bounds this, not a clock. Injections are on:
    // without them a fenced block in Markdown and HTML's `<script>` and
    // `<style>` were all plain text.
    // A closure and not `injected` itself: the function item would pin the
    // callback's lifetime to `'static`, and with it `text` and `config`.
    #[allow(clippy::redundant_closure)]
    let events = highlighter
        .highlight(config, text.as_bytes(), None, None, |name| injected(name))
        .ok()?;

    let mut spans = Vec::new();
    // A stack, because captures nest: a string inside a macro inside a
    // function. The innermost is the one that wins, which is what `last` is.
    let mut stack: Vec<usize> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(highlight) => stack.push(highlight.0),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                spans.push((start, end, stack.last().copied().and_then(class_of)));
            }
        }
    }
    Some(spans)
}

/// Markdown in two passes, where every other language takes one.
///
/// Markdown is two grammars: the block one (headings, lists, fences) and the
/// inline one (emphasis, code spans, links), which the block grammar's query
/// hands every `inline` node to by injection. `tree_sitter_highlight` runs
/// that injection and gets **nothing back**. It parses correctly with the
/// same ranges, and it highlights correctly on its own. Injected through a
/// fence instead, it is empty again, so the failure is the inline grammar as
/// an injected layer, not the block query. So the inline pass is run here
/// instead: each `inline` node's text is highlighted on its own and laid over
/// the block pass, winning wherever it has something to say.
///
/// One approximation: a paragraph inside a block quote carries its `> `
/// markers inside the `inline` node's text, and the inline grammar reads them
/// as text. They were plain before this, and they are plain now.
fn markdown(text: &str) -> Option<Vec<Vec<Chunk>>> {
    let block = GRAMMARS.get("markdown")?;
    let inline = GRAMMARS.get("markdown_inline")?;

    let mut kinds: Vec<Option<&'static str>> = vec![None; text.len()];
    for (start, end, kind) in spans(text, block)? {
        kinds[start..end].fill(kind);
    }

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_md::LANGUAGE.into()).ok()?;
    let tree = parser.parse(text, None)?;
    let mut cursor = tree.walk();
    let mut visit = true;
    loop {
        let node = cursor.node();
        if visit && node.kind() == "inline" {
            let offset = node.start_byte();
            for (start, end, kind) in spans(text.get(node.byte_range())?, inline)? {
                if kind.is_some() {
                    kinds[offset + start..offset + end].fill(kind);
                }
            }
        }
        if visit && node.kind() != "inline" && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
            continue;
        }
        if !cursor.goto_parent() {
            break;
        }
        visit = false;
    }

    // Back into runs. A boundary only ever falls where a capture began or
    // ended, which is a character boundary; `cut` refuses anything else, and
    // the file is then served as text rather than split mid-character.
    let mut runs = Vec::new();
    let mut start = 0;
    for index in 1..=kinds.len() {
        if index == kinds.len() || kinds[index] != kinds[start] {
            runs.push((start, index, kinds[start]));
            start = index;
        }
    }
    cut(text, &runs)
}

/// Cuts runs into lines.
///
/// The fiddly half: a run may cross newlines (a block comment, a multi-line
/// string), and a line is what the viewer lays out. Each run is split on `\n`
/// and its pieces are pushed onto the line they belong to, so a chunk never
/// contains one.
fn cut(text: &str, spans: &[Span]) -> Option<Vec<Vec<Chunk>>> {
    let mut lines: Vec<Vec<Chunk>> = vec![Vec::new()];
    for &(start, end, kind) in spans {
        let piece = text.get(start..end)?;
        for (index, part) in piece.split('\n').enumerate() {
            if index > 0 {
                lines.push(Vec::new());
            }
            if part.is_empty() {
                continue;
            }
            let current = lines.last_mut()?;
            // Merged with the previous chunk when they agree, so a line of
            // code is a handful of spans rather than one per token the parser
            // happened to emit separately.
            match current.last_mut() {
                Some(last) if last.kind == kind => last.text.push_str(part),
                _ => current.push(Chunk {
                    text: part.to_string(),
                    kind,
                }),
            }
        }
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_gets_a_grammar_and_a_keyword() {
        let (lang, lines) = lines("a/b/main.rs", "fn main() {}\n");
        assert_eq!(lang, Some("rust"));
        let kinds: Vec<_> = lines[0].iter().map(|c| (c.text.as_str(), c.kind)).collect();
        assert!(
            kinds
                .iter()
                .any(|(text, kind)| *text == "fn" && *kind == Some("keyword")),
            "expected `fn` to be a keyword, got {kinds:?}",
        );
    }

    /// The property every consumer depends on, and the one the line-splitting
    /// is most likely to break: whatever comes back, joining it must be the
    /// file again.
    #[test]
    fn the_chunks_join_back_into_the_file() {
        let source = "/* a comment\n   over two lines */\nfn f() {\n    let s = \"ñ á\";\n}\n";
        let (_, lines) = lines("x.rs", source);
        let rebuilt = lines
            .iter()
            .map(|line| line.iter().map(|c| c.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rebuilt, source);
    }

    #[test]
    fn no_chunk_contains_a_newline() {
        let (_, lines) = lines("x.rs", "fn a() {\n    // hi\n}\n");
        for line in &lines {
            for chunk in line {
                assert!(
                    !chunk.text.contains('\n'),
                    "chunk spans a line break: {chunk:?}"
                );
            }
        }
    }

    /// A file with no grammar and a file too big to parse take the same path
    /// out as one that highlighted, so the viewer never special-cases them.
    #[test]
    fn an_unknown_language_still_comes_back_as_lines() {
        let (lang, lines) = lines("notes.unknownext", "one\ntwo\n");
        assert_eq!(lang, None);
        assert_eq!(lines.len(), 3, "trailing newline is an empty last line");
        assert_eq!(lines[0][0].text, "one");
        assert!(lines[0][0].kind.is_none());
    }

    fn kind_of<'a>(lines: &'a [Vec<Chunk>], text: &str) -> Option<&'a str> {
        lines
            .iter()
            .flatten()
            .find(|chunk| chunk.text.contains(text))
            .and_then(|chunk| chunk.kind)
    }

    /// Markdown's captures are prose names (`text.title`, `text.literal`), and
    /// until they were listed a README came back as its punctuation.
    #[test]
    fn markdown_gets_headings_code_and_links() {
        let source =
            "# Title here\n\nSome *soft* and **loud** with `code` and [a link](https://x.y).\n";
        let (lang, lines) = lines("README.md", source);
        assert_eq!(lang, Some("markdown"));
        assert_eq!(kind_of(&lines, "Title here"), Some("heading"));
        assert_eq!(kind_of(&lines, "soft"), Some("emphasis"));
        assert_eq!(kind_of(&lines, "loud"), Some("strong"));
        assert_eq!(kind_of(&lines, "code"), Some("literal"));
        assert_eq!(kind_of(&lines, "https://x.y"), Some("link"));
    }

    /// The two passes must still add up to the file, byte for byte, through
    /// multi-byte text, a block quote and a fence.
    #[test]
    fn markdown_chunks_join_back_into_the_file() {
        let source = "# Añadir *más* — `ñ`\n\n> citado **fuerte**\n> segunda línea\n\n- uno\n- [dos](x.md)\n\n```rust\nlet s = \"é\";\n```\n";
        let (lang, lines) = lines("x.md", source);
        assert_eq!(lang, Some("markdown"));
        let rebuilt = lines
            .iter()
            .map(|line| line.iter().map(|c| c.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rebuilt, source);
        assert_eq!(kind_of(&lines, "fuerte"), Some("strong"));
    }

    /// A fenced block is its own language, by injection, under either name.
    #[test]
    fn a_fenced_block_in_markdown_is_highlighted_as_its_language() {
        for fence in ["rust", "rs"] {
            let source = format!("Text.\n\n```{fence}\nfn main() {{}}\n```\n");
            let (_, lines) = lines("notes.md", &source);
            assert_eq!(
                kind_of(&lines, "fn"),
                Some("keyword"),
                "fence `{fence}`: {lines:?}"
            );
        }
    }

    /// A reply's snippet is found by the same names a Markdown fence is, and
    /// says which language it was taken for; an unknown one is plain text.
    #[test]
    fn a_reply_snippet_is_highlighted_by_its_fence() {
        for fence in ["rust", "rs", " Rust "] {
            let (language, lines) = fenced(fence, "fn main() {}");
            assert_eq!(language, Some("rust"), "fence `{fence}`");
            assert_eq!(kind_of(&lines, "fn"), Some("keyword"), "fence `{fence}`");
        }
        let (language, _) = fenced("shell", "echo hi");
        assert_eq!(language, Some("bash"));
        let (language, lines) = fenced("nonesuch", "fn main() {}");
        assert_eq!(language, None);
        assert_eq!(kind_of(&lines, "fn"), None);
        assert_eq!(fenced("", "x").0, None);
    }

    /// TypeScript's query inherits JavaScript's; alone it found two keywords
    /// in a whole file.
    #[test]
    fn typescript_inherits_javascript_highlighting() {
        let (lang, ts) = lines(
            "a.ts",
            "const x: number = 1\nfunction f() { return \"s\" }\n",
        );
        assert_eq!(lang, Some("typescript"));
        assert_eq!(kind_of(&ts, "const"), Some("keyword"));
        assert_eq!(kind_of(&ts, "return"), Some("keyword"));
        assert_eq!(kind_of(&ts, "\"s\""), Some("string"));
        let (lang, _) = lines("a.tsx", "const a = <div className=\"x\" />\n");
        assert_eq!(lang, Some("tsx"));
    }

    /// HTML hands `<script>` to JavaScript by injection, and so does every
    /// component in `web/`.
    #[test]
    fn a_script_inside_html_is_highlighted_as_javascript() {
        let (_, lines) = lines("c.html", "<script>\n  const n = 1\n</script>\n");
        assert_eq!(kind_of(&lines, "const"), Some("keyword"));
    }

    /// Neither has an extension, so both are found by name.
    #[test]
    fn a_containerfile_and_a_makefile_are_found_by_name() {
        let source = "FROM rust:1 AS build\nRUN cargo build --release\n";
        for name in ["Containerfile", "Dockerfile", "a/app.dockerfile"] {
            let (lang, lines) = lines(name, source);
            assert_eq!(lang, Some("dockerfile"), "{name}");
            assert_eq!(kind_of(&lines, "FROM"), Some("keyword"), "{name}");
        }
        let source = "include x.mk\nCC ?= cc\n\nbuild: main.c\n\t$(CC) -o main main.c\n";
        for name in ["Makefile", "GNUmakefile", "a/rules.mk"] {
            let (lang, lines) = lines(name, source);
            assert_eq!(lang, Some("make"), "{name}");
            assert_eq!(kind_of(&lines, "include"), Some("keyword"), "{name}");
            assert_eq!(kind_of(&lines, "build"), Some("function"), "{name}");
        }
    }

    /// A grammar whose query fails to compile is dropped silently, by design,
    /// so this is the only place that notices one did.
    #[test]
    fn every_grammar_compiles() {
        for name in [
            "rust",
            "javascript",
            "typescript",
            "tsx",
            "html",
            "css",
            "toml",
            "markdown",
            "markdown_inline",
            "json",
            "python",
            "go",
            "c",
            "cpp",
            "yaml",
            "bash",
            "dockerfile",
            "make",
        ] {
            assert!(GRAMMARS.contains_key(name), "`{name}` did not compile");
        }
    }

    #[test]
    fn a_file_over_the_cap_is_served_as_text() {
        let big = "fn main() {}\n".repeat(MAX_HIGHLIGHT_BYTES / 8);
        let (lang, lines) = lines("big.rs", &big);
        assert_eq!(lang, None, "too big to parse, and that is not an error");
        assert!(!lines.is_empty());
    }
}
