//! What the agent can do, and the one place it is checked.
//!
//! The model never executes anything. It emits a structured request; this
//! module parses it, validates it against the [`Sandbox`], and runs real Rust.
//! Any path where model output reaches a shell or the filesystem without
//! passing that validation is a bug, however convenient.
//!
//! **The definitions live in the cached prefix**, so [`Tools::definitions`] is a
//! wire format: tools sorted by name, schemas serialized through `serde_json`'s
//! sorted maps, nothing interpolated. Reordering the set or re-serializing a
//! schema with different key order costs the whole prompt cache on every call,
//! and nothing fails — it just gets slower.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::sandbox::{Access, Sandbox, Verdict};

pub mod command;
pub mod fs;

pub use command::RunCommand;
pub use fs::{EditFile, ListDir, ReadFile, WriteFile};

/// Tool output is capped rather than pruned. Pruning old results out of the
/// history is a later, measured change; a cap now is the part that is not a
/// strategy — a `cat` of a 2 MB file must not be able to blow the window open.
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024;

/// What the model asked for, whatever syntax it used to ask.
///
/// The type is the interface. How a model expresses a call is a transport
/// detail that differs per backend — a fenced block today, native function
/// calling and a GBNF grammar next — and the loop must not care which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// What a subprocess did, beyond what it printed.
///
/// Every field here existed as a fact and survived as a sentence: the exit code
/// lived for one moment inside `"{program} exited with {code}"`, the two streams
/// were concatenated into one blob under `--- stdout` headers, and the duration
/// was never captured at all. That is fine for something a model reads and
/// nothing else, and it is the wrong shape for everything above it — **a task
/// cannot be closed on an exit code that was never a field**, which is the next
/// rung of the ladder `protocol.rs` describes above `CloseTask`.
///
/// So: the type and the record are structured, and [`ToolOutcome::render`]
/// stays as frugal as it was. A 7B pays for every token of a wrapper it did not
/// need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    /// `None` when a signal ended the child, which is exactly when `signal` is
    /// `Some` — the two together are the whole of `wait`'s answer.
    pub exit_code: Option<i32>,
    /// The signal that ended it, if one did. This is how a run says *which*
    /// limit stopped a child: `SIGXCPU` and `SIGXFSZ` are `[sandbox.limits]`
    /// arriving, and nothing else in the outcome could tell them from a crash.
    pub signal: Option<i32>,
    /// Separately, because a judge that has to re-split one blob on `---
    /// stdout` is parsing a rendering. Capped like `output` is: a recording is
    /// not a place to keep 2 MB of somebody's build log.
    pub stdout: String,
    pub stderr: String,
    /// How long the child ran. Distinct from the step's own duration, which
    /// includes the checks and the process spawn.
    pub duration_ms: u64,
}

impl CommandResult {
    /// How a signal reads, for the one sentence that mentions it.
    ///
    /// Only the signals a limit or a kill produces are named; anything else is
    /// its number, which is honest and does not pretend to a table this does
    /// not need.
    pub fn signal_name(signal: i32) -> String {
        #[cfg(unix)]
        {
            let named = match signal {
                libc::SIGXCPU => Some("SIGXCPU: the cpu-seconds limit"),
                libc::SIGXFSZ => Some("SIGXFSZ: the file-size-mb limit"),
                libc::SIGKILL => Some("SIGKILL"),
                libc::SIGSEGV => Some("SIGSEGV"),
                libc::SIGABRT => Some("SIGABRT"),
                libc::SIGTERM => Some("SIGTERM"),
                _ => None,
            };
            if let Some(named) = named {
                return format!("{named}, signal {signal}");
            }
        }
        format!("signal {signal}")
    }
}

/// What running it produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub verdict: Verdict,
    /// What the model gets back. Empty on a denial: the verdict is the answer.
    pub output: String,
    /// The tool ran and failed — a missing file, a non-zero exit. Distinct from
    /// a denial, which never ran, and the model needs to tell them apart to
    /// decide whether trying something else is worth a turn.
    pub error: Option<String>,
    pub truncated: bool,
    /// Present for `run_command` and absent for everything else, which is the
    /// honest shape: an in-process tool has no exit code, and a zero would be
    /// a lie about a fact that does not exist.
    ///
    /// Additive on the wire and in the record — `#[serde(default)]`, absent
    /// means "a recording from before this existed", which is why the format
    /// number did not move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandResult>,
}

impl ToolOutcome {
    pub fn ok(verdict: Verdict, output: impl Into<String>) -> Self {
        let (output, truncated) = clamp(output.into());
        Self {
            verdict,
            output,
            error: None,
            truncated,
            command: None,
        }
    }

    /// The same, carrying what the subprocess did.
    pub fn from_command(
        verdict: Verdict,
        output: impl Into<String>,
        command: CommandResult,
    ) -> Self {
        Self {
            command: Some(command),
            ..Self::ok(verdict, output)
        }
    }

    pub fn failed(verdict: Verdict, error: impl Into<String>) -> Self {
        Self {
            verdict,
            output: String::new(),
            error: Some(error.into()),
            truncated: false,
            command: None,
        }
    }

    pub fn denied(verdict: Verdict) -> Self {
        let error = format!("denied: {}", verdict.rule);
        Self {
            verdict,
            output: String::new(),
            error: Some(error),
            truncated: false,
            command: None,
        }
    }

    /// How the result is rendered back into the conversation.
    ///
    /// Plain text and not JSON: a 7B pays for every token of a wrapper it did
    /// not need, and this one is read, never parsed.
    pub fn render(&self, name: &str) -> String {
        let mut text = match &self.error {
            Some(error) => format!("[{name}] {error}"),
            None => format!("[{name}] ok"),
        };
        if !self.output.is_empty() {
            text.push('\n');
            text.push_str(&self.output);
        }
        if self.truncated {
            text.push_str(&format!(
                "\n[{name}] output cut at {MAX_OUTPUT_BYTES} bytes"
            ));
        }
        text
    }
}

/// Cuts at a character boundary, so the result is still a string.
pub(crate) fn clamp(mut text: String) -> (String, bool) {
    if text.len() <= MAX_OUTPUT_BYTES {
        return (text, false);
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

/// One call and what came back, as it is kept in the history.
///
/// `text` is the assistant's own words verbatim rather than a re-rendering of
/// `call`: it is what goes back into the prompt, and a prompt whose earlier
/// turns are regenerated rather than replayed is a different prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStep {
    pub text: String,
    pub call: ToolCall,
    pub outcome: ToolOutcome,
    pub duration_ms: u64,
    /// What a person answered when the floor refused this call and the policy
    /// file would have allowed it: `true` for *allow once*, `false` for *deny*.
    /// `None` when nobody was asked, which is every call that was not refused
    /// that way and every step recorded before asking existed. See
    /// `RECORD/2026-10-02.a-refused-write-asks.completed.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked: Option<bool>,
}

impl ToolStep {
    /// The user-side message the result is fused into.
    pub fn result_text(&self) -> String {
        self.outcome.render(&self.call.name)
    }
}

/// Whether `sandbox` would let `call` write the one path it names — the pure
/// check that decides if a refused call is worth putting to a person.
///
/// `write_file` and `edit_file` and nothing else. Both check the path before
/// touching anything, which is what makes running one again after a refusal
/// sound. `run_command` never qualifies: the floor keeps the session's commands,
/// so it refuses a command's writes at a syscall, where there is no call left
/// to pause.
pub fn writes_one_path(call: &ToolCall, sandbox: &Sandbox) -> bool {
    matches!(call.name.as_str(), "write_file" | "edit_file")
        && call
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|path| {
                sandbox
                    .check_path(std::path::Path::new(path), Access::ReadWrite)
                    .verdict
                    .allowed
            })
}

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;

/// One thing the agent can do.
///
/// Object-safe on purpose — the registry holds `dyn Tool`, so adding a tool
/// never reaches the loop. `Pin<Box<dyn Future>>` rather than an `async fn`
/// for the same reason [`crate::backend::Backend`] returns a boxed stream.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    /// One line, in the prompt, paid for on every call. Write it that way.
    fn description(&self) -> &'static str;
    /// JSON Schema for the arguments.
    fn parameters(&self) -> serde_json::Value;
    /// Runs it. Every implementation asks the sandbox first; there is no path
    /// through here that does not.
    fn run<'a>(&'a self, arguments: &'a serde_json::Value, sandbox: &'a Sandbox) -> ToolFuture<'a>;
}

/// The tool set, and its rendering into the stable prefix.
pub struct Tools {
    tools: Vec<Box<dyn Tool>>,
}

impl Default for Tools {
    fn default() -> Self {
        Self::standard()
    }
}

impl Tools {
    /// Sorted by name at construction, because the order is part of the cached
    /// prefix and the declaration order of a `vec!` is not a contract.
    pub fn new(mut tools: Vec<Box<dyn Tool>>) -> Self {
        tools.sort_by_key(|tool| tool.name());
        Self { tools }
    }

    pub fn standard() -> Self {
        Self::new(vec![
            Box::new(ReadFile),
            Box::new(ListDir),
            Box::new(EditFile),
            Box::new(WriteFile),
            Box::new(RunCommand),
        ])
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        self.tools.iter().map(|tool| tool.name())
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(AsRef::as_ref)
    }

    /// The tools as the native transport sends them, in the order
    /// [`Self::definitions`] renders them — the same three facts each tool
    /// declares, so there is no second definition of a tool's shape.
    pub fn specs(&self) -> Vec<crate::backend::ToolSpec> {
        self.tools
            .iter()
            .map(|tool| crate::backend::ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters(),
            })
            .collect()
    }

    /// What the native transport sends in the request's `tools` field, as the
    /// string the context counts in its `tools` bucket. Empty when there are
    /// no tools, like [`Self::definitions`].
    pub fn native_definitions(&self) -> String {
        if self.tools.is_empty() {
            return String::new();
        }
        serde_json::to_string(&crate::backend::native::tools(&self.specs()))
            .expect("a schema built from json! always serializes")
    }

    /// The block appended to the system text. Byte-stable across calls, across
    /// processes, and across the order the tools were declared in.
    pub fn definitions(&self) -> String {
        if self.tools.is_empty() {
            return String::new();
        }

        let mut text = String::from(PREAMBLE);
        for tool in &self.tools {
            text.push_str(&format!(
                "\n{} — {}\n{}\n",
                tool.name(),
                tool.description(),
                serde_json::to_string(&tool.parameters())
                    .expect("a schema built from json! always serializes"),
            ));
        }
        text
    }

    /// A JSON Schema for exactly one call to exactly one of these tools —
    /// `{"name": "read_file", "arguments": {...}}`, `ToolCall`'s own shape,
    /// as a `oneOf` over every tool's own `parameters()`. No second
    /// definition of a tool's shape, for the reason [`fenced`]'s own comment
    /// gives about two scanners drifting.
    ///
    /// This is the *retry* arm `RECORD/2026-09-06.a-grammar-for-tool-calls.completed.md`
    /// names beside the grammar: sent as a server's `response_format`, every
    /// reply becomes a call — there is no "just answer" branch, unlike
    /// [`crate::grammar::compile`]'s. Sending it on a first attempt would
    /// make every turn a tool call whether or not one was wanted; sending it
    /// on a second attempt, after an unconstrained first one drifted, is the
    /// shape that keeps "just answer" reachable at all.
    pub fn call_schema(&self) -> serde_json::Value {
        let one_of: Vec<serde_json::Value> = self
            .tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"const": tool.name()},
                        "arguments": tool.parameters(),
                    },
                    "required": ["name", "arguments"],
                })
            })
            .collect();
        serde_json::json!({ "oneOf": one_of })
    }

    /// Runs a call, or says why it did not.
    pub async fn call(&self, call: &ToolCall, sandbox: &Sandbox) -> ToolOutcome {
        match self.get(&call.name) {
            Some(tool) => tool.run(&call.arguments, sandbox).await,
            None => ToolOutcome::denied(Verdict::deny(format!(
                "there is no tool called `{}`; the tools are {}",
                call.name,
                self.names().collect::<Vec<_>>().join(", ")
            ))),
        }
    }
}

/// The instructions, kept next to the rendering they are part of. Every byte
/// here is in the cached prefix and in every prompt.
const PREAMBLE: &str = "\
# Tools

To use one, end your reply with a fenced block and nothing after it:

```tool
{\"name\": \"read_file\", \"arguments\": {\"path\": \"src/main.rs\"}}
```

The result comes back as the next message. One call per reply. When no tool is
needed, answer in plain text and do not emit a block.
";

/// Reads a tool call out of what the model produced.
///
/// The one function that changes when the backend learns native function
/// calling; nothing above it does. It accepts the fenced form the preamble asks
/// for, and a bare JSON object, because a 7B that has never seen a tool API
/// drops the fence about a third of the time and refusing that would be
/// measuring the fence rather than the loop.
///
/// **And a ```` ```json ```` fence**, which is where a model given its own
/// trained tool prompt — the native transport, where the server's template
/// renders the tools — puts the call when it does not write the template's
/// tags: `qwen2.5-coder:7b` did so on every call it made, measured. The bare
/// scan already found the object inside it; naming the fence is what lets the
/// page and the native history treat the block as one unit. See
/// `RECORD/2026-09-30.native-tool-calls.completed.md`.
pub fn parse_call(text: &str) -> Option<ToolCall> {
    split_call(text).map(|(_, call)| call)
}

/// The fences a call may arrive under: ours, then the one a model trained on a
/// native tool prompt writes instead of its template's tags.
pub const CALL_FENCES: [&str; 2] = ["tool", "json"];

/// [`parse_call`], plus what the reply said **before** the call — the prose
/// half of an assistant message whose call the native transport sends as
/// `tool_calls`. Whatever follows the call is not kept: a call ends a reply,
/// and text after it is the model writing its own result
/// ([`CallVerdict::ContinuedPastFence`]), which the history should not tell it
/// was said.
pub fn split_call(text: &str) -> Option<(String, ToolCall)> {
    let fenced = CALL_FENCES.iter().find_map(|tag| {
        let (open, body, _) = fenced_bounds(text, tag)?;
        let call = serde_json::from_str::<ToolCall>(body).ok()?;
        Some((open, call))
    });
    let (start, call) = fenced.or_else(|| bare_object_at(text))?;
    (!call.name.is_empty()).then(|| (text[..start].trim_end().to_string(), call))
}

/// Where a reply landed against the tool-call format, for a probe that
/// scores one call per prompt. `parse_call` answers *is this a call*;
/// `score_call` answers the four-way question roadmap item 4 asked for and
/// no scanner in this crate counted before — see
/// `RECORD/2026-09-14.the-tool-call-probe.completed.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallVerdict {
    /// A call parsed, and nothing followed it.
    Parsed,
    /// A call parsed inside a ```tool fence, and the model kept writing after
    /// the closing fence — the precision run's failure, found by accident:
    /// ~1 700 invented characters of the call's own result, streamed and
    /// stored, because the fenced form has no way to tell a model to stop.
    ContinuedPastFence,
    /// A fenced block tagged with a tool's own name (```write_file) instead
    /// of the format (```tool {"name": "write_file", ...}) — an attempt that
    /// missed the shape, not a decision not to call.
    Drifted,
    /// No call attempted; an ordinary prose answer.
    NoCall,
}

/// Scores one reply. `tool_names` is the tool set the drift check looks for a
/// misnamed fence against — the caller's, because this function has no
/// [`Tools`] of its own and should not need one to be tested.
pub fn score_call(text: &str, tool_names: &[&str]) -> CallVerdict {
    if let Some((_, close_end)) = CALL_FENCES
        .iter()
        .filter_map(|tag| fenced_span(text, tag))
        .find(|(body, _)| {
            serde_json::from_str::<ToolCall>(body)
                .ok()
                .is_some_and(|call: ToolCall| !call.name.is_empty())
        })
    {
        return match text[close_end..].trim().is_empty() {
            true => CallVerdict::Parsed,
            false => CallVerdict::ContinuedPastFence,
        };
    }
    if tool_names.iter().any(|name| fenced(text, name).is_some()) {
        return CallVerdict::Drifted;
    }
    match bare_object(text).is_some_and(|call| !call.name.is_empty()) {
        true => CallVerdict::Parsed,
        false => CallVerdict::NoCall,
    }
}

/// The body of the first ```tool block, if the block is closed. An unclosed one
/// is a call still being generated, and half a call is not a call.
/// The body of the first ```` ```<tag> ```` block, if there is one.
///
/// Shared with [`crate::task::parse_plan`]: a plan and a call are the same
/// transport problem, and two scanners would drift on the same malformed
/// block.
pub(crate) fn fenced<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    fenced_span(text, tag).map(|(body, _)| body)
}

/// Like [`fenced`], plus the byte offset in `text` right after the closing
/// fence — for a caller that cares what came *after* the block, which
/// `fenced` itself never needs to.
fn fenced_span<'a>(text: &'a str, tag: &str) -> Option<(&'a str, usize)> {
    fenced_bounds(text, tag).map(|(_, body, close_end)| (body, close_end))
}

/// Like [`fenced_span`], plus where the opening fence starts.
fn fenced_bounds<'a>(text: &'a str, tag: &str) -> Option<(usize, &'a str, usize)> {
    let fence = format!("```{tag}");
    let open = text.find(&fence)?;
    let rest = &text[open + fence.len()..];
    let (skipped, body_region) = match rest.strip_prefix('\n') {
        Some(r) => (1, r),
        None => (0, rest),
    };
    let close = body_region.find("```")?;
    let body = body_region[..close].trim();
    let close_end = open + fence.len() + skipped + close + "```".len();
    Some((open, body, close_end))
}

/// The first `{…}` that parses as a call. Scanned from every `{` rather than
/// from the first, because models like to open with prose containing braces.
fn bare_object(text: &str) -> Option<ToolCall> {
    bare_object_at(text).map(|(_, call)| call)
}

/// [`bare_object`], and where the object starts.
fn bare_object_at(text: &str) -> Option<(usize, ToolCall)> {
    let bytes = text.as_bytes();
    for (start, _) in text
        .char_indices()
        .filter(|(i, c)| *c == '{' && bytes.get(i.wrapping_sub(1)).is_none_or(|b| *b != b'{'))
    {
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<ToolCall>();
        if let Some(Ok(call)) = stream.next() {
            return Some((start, call));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fenced_call_parses() {
        let call = parse_call(
            "I will look at the file.\n\n```tool\n{\"name\": \"read_file\", \
             \"arguments\": {\"path\": \"src/main.rs\"}}\n```\n",
        )
        .unwrap();
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments["path"], "src/main.rs");
    }

    #[test]
    fn a_call_without_the_fence_still_parses() {
        let call =
            parse_call("Sure. {\"name\":\"list_dir\",\"arguments\":{\"path\":\".\"}}").unwrap();
        assert_eq!(call.name, "list_dir");
    }

    #[test]
    fn prose_with_braces_before_the_call_does_not_stop_it_being_found() {
        let call =
            parse_call("The struct is `Foo { bar }`, so: {\"name\":\"list_dir\",\"arguments\":{}}")
                .unwrap();
        assert_eq!(call.name, "list_dir");
    }

    #[test]
    fn an_unclosed_block_is_not_a_call() {
        // It is a call still being generated. Executing half of one is how a
        // streaming agent runs something nobody asked for.
        assert!(parse_call("```tool\n{\"name\": \"run_command\"").is_none());
    }

    #[test]
    fn plain_prose_is_an_answer_and_not_a_call() {
        assert!(parse_call("The file defines one function, `main`.").is_none());
    }

    const TOOL_NAMES: &[&str] = &[
        "read_file",
        "list_dir",
        "run_command",
        "write_file",
        "edit_file",
    ];

    #[test]
    fn score_call_parses_a_clean_fenced_call() {
        let text = "looking\n```tool\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"src/main.rs\"}}\n```";
        assert_eq!(score_call(text, TOOL_NAMES), CallVerdict::Parsed);
    }

    #[test]
    fn score_call_parses_a_bare_object_with_no_fence_to_have_continued_past() {
        let text = "Sure. {\"name\":\"list_dir\",\"arguments\":{\"path\":\".\"}}";
        assert_eq!(score_call(text, TOOL_NAMES), CallVerdict::Parsed);
    }

    #[test]
    fn score_call_catches_generation_that_does_not_stop_at_the_fence() {
        // The precision run's failure, reproduced: the block parses, and then
        // the model writes the tool's own result itself.
        let text = "looking\n```tool\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}\n```\nThe file defines `run_agent_turn`, which loops until a call...";
        assert_eq!(
            score_call(text, TOOL_NAMES),
            CallVerdict::ContinuedPastFence
        );
    }

    #[test]
    fn score_call_catches_a_call_fenced_under_the_tools_own_name() {
        // The gate probe's own failure: ```write_file instead of
        // ```tool {"name": "write_file", ...}.
        let text = "```write_file\n{\"path\": \"a.rs\", \"content\": \"fn main() {}\"}\n```";
        assert_eq!(score_call(text, TOOL_NAMES), CallVerdict::Drifted);
    }

    #[test]
    fn score_call_is_no_call_for_plain_prose() {
        assert_eq!(
            score_call("The file defines one function, `main`.", TOOL_NAMES),
            CallVerdict::NoCall
        );
    }

    #[test]
    fn the_definitions_are_byte_identical_however_the_set_was_declared() {
        // The cached prefix is a wire format. Two processes that built the same
        // tool set differently must send the same bytes.
        let one = Tools::new(vec![Box::new(ReadFile), Box::new(ListDir)]).definitions();
        let other = Tools::new(vec![Box::new(ListDir), Box::new(ReadFile)]).definitions();
        assert_eq!(one, other);
        assert_eq!(
            one,
            Tools::new(vec![Box::new(ReadFile), Box::new(ListDir)]).definitions()
        );
    }

    #[test]
    fn schema_keys_serialize_in_a_fixed_order() {
        // This is the guard against a transitive dependency turning on
        // `serde_json/preserve_order`: the maps would become insertion-ordered,
        // every schema would render in declaration order, and the only symptom
        // would be a prompt cache that quietly stopped hitting.
        let forwards = serde_json::json!({"a": 1, "b": 2, "c": 3});
        let backwards = serde_json::json!({"c": 3, "b": 2, "a": 1});
        assert_eq!(
            serde_json::to_string(&forwards).unwrap(),
            serde_json::to_string(&backwards).unwrap(),
        );
    }

    #[test]
    fn the_standard_set_lists_every_tool_it_documents() {
        let tools = Tools::standard();
        let definitions = tools.definitions();
        for name in tools.names() {
            assert!(definitions.contains(name), "{name} is not in the prefix");
        }
        assert!(tools.get("no_such_tool").is_none());
    }

    #[test]
    fn output_is_cut_at_a_character_boundary_and_says_so() {
        let outcome = ToolOutcome::ok(
            Verdict::allow("test", crate::sandbox::Applied::Process),
            "ñ".repeat(MAX_OUTPUT_BYTES),
        );
        assert!(outcome.truncated);
        assert!(outcome.output.len() <= MAX_OUTPUT_BYTES);
        assert!(outcome.render("read_file").contains("cut at"));
    }
}
