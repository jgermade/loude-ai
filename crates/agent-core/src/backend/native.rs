//! The native tool transport, both directions, in one place.
//!
//! Everything above the backend renders a tool step the way it always has: an
//! assistant message holding the call as text, then a user message holding its
//! result, `[name] …`. That pair is an invariant the design already keeps — a
//! turn is evicted whole, so a result never loses its call — which is what lets
//! this module rebuild the API's shape from it rather than every layer above
//! carrying `tool_calls` through `Message`, the context, the store and the
//! protocol:
//!
//! - **out**, [`messages`]: an assistant message whose text holds a call,
//!   followed by the user message that is that call's result, becomes an
//!   assistant message with `tool_calls` (its content the prose before the
//!   call) and a `tool` message. Anything else goes as it is.
//! - **in**, [`Calls`] and [`fence`]: a call the server itself parsed comes
//!   back up as the canonical fenced text, so `parse_call` finds it where it
//!   finds every other call. A call the server did *not* parse — a model that
//!   wrote a ```` ```json ```` block instead of its template's tags — is text
//!   already.
//!
//! See `RECORD/2026-09-30.native-tool-calls.completed.md`.

use serde::Serialize;

use super::{Message, Role, ToolSpec};
use crate::tools::{ToolCall, split_call};

/// How a server wants a call's arguments: the OpenAI API as a JSON-encoded
/// *string*, Ollama's own API as the object itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arguments {
    String,
    Object,
}

/// One message as a server with a `tools` field reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireMessage {
    pub role: &'static str,
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<WireCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireFunction {
    pub name: String,
    pub arguments: serde_json::Value,
}

fn role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// The history as the API spells it. See the module note for the one
/// conversion this makes, and why it can be found rather than guessed: the
/// result message has to open with the call's own `[name]`, which is how
/// `ToolOutcome::render` begins every result.
pub fn messages(history: &[Message], arguments: Arguments) -> Vec<WireMessage> {
    let mut out = Vec::with_capacity(history.len());
    let mut calls = 0;
    let mut i = 0;
    while i < history.len() {
        let message = &history[i];
        let paired = (message.role == Role::Assistant)
            .then(|| split_call(&message.content))
            .flatten()
            .filter(|(_, call)| {
                history.get(i + 1).is_some_and(|next| {
                    next.role == Role::User && next.content.starts_with(&format!("[{}]", call.name))
                })
            });
        match paired {
            Some((prose, call)) => {
                calls += 1;
                let id = format!("call_{calls}");
                out.push(WireMessage {
                    role: "assistant",
                    content: prose,
                    tool_calls: vec![WireCall {
                        id: id.clone(),
                        kind: "function",
                        function: WireFunction {
                            name: call.name,
                            arguments: match arguments {
                                Arguments::Object => call.arguments,
                                Arguments::String => {
                                    serde_json::Value::String(call.arguments.to_string())
                                }
                            },
                        },
                    }],
                    tool_call_id: None,
                });
                out.push(WireMessage {
                    role: "tool",
                    content: history[i + 1].content.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(id),
                });
                i += 2;
            }
            None => {
                out.push(WireMessage {
                    role: role(message.role),
                    content: message.content.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
                i += 1;
            }
        }
    }
    out
}

/// The `tools` field: the same shape on the OpenAI API and on Ollama's.
pub fn tools(specs: &[ToolSpec]) -> Vec<serde_json::Value> {
    specs
        .iter()
        .map(|spec| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": spec.parameters,
                },
            })
        })
        .collect()
}

/// A call the server parsed, as the text everything above the backend reads —
/// the fence the fenced transport asks for, so a stored turn reads the same
/// whichever transport carried it. `after_text` puts a blank line between it
/// and prose the reply already streamed.
pub fn fence(call: &ToolCall, after_text: bool) -> String {
    // Written out rather than through `json!`, whose map sorts its keys: the
    // name first, as the fenced format's own example puts it.
    format!(
        "{}```tool\n{{\"name\":{},\"arguments\":{}}}\n```",
        if after_text { "\n\n" } else { "" },
        serde_json::Value::String(call.name.clone()),
        call.arguments,
    )
}

/// Calls streamed in pieces, by index — the OpenAI API's shape: the first
/// delta for an index carries the name, the ones after it carry fragments of
/// the arguments' JSON string.
#[derive(Debug, Default)]
pub struct Calls {
    calls: Vec<(String, String)>,
}

impl Calls {
    pub fn push(&mut self, index: usize, name: Option<&str>, arguments: Option<&str>) {
        if self.calls.len() <= index {
            self.calls.resize(index + 1, (String::new(), String::new()));
        }
        let (n, a) = &mut self.calls[index];
        if let Some(name) = name {
            n.push_str(name);
        }
        if let Some(arguments) = arguments {
            a.push_str(arguments);
        }
    }

    /// Every call that got a name. Arguments that do not parse are kept as
    /// the string they were, so the tool refuses them by name rather than the
    /// call vanishing.
    pub fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, arguments)| ToolCall {
                name,
                arguments: match arguments.trim() {
                    "" => serde_json::json!({}),
                    raw => serde_json::from_str(raw)
                        .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_becomes_tool_calls_and_a_tool_message() {
        let history = vec![
            Message::system("sys"),
            Message::user("read it"),
            Message::assistant(
                "Sure.\n```json\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"README.md\"}}\n```",
            ),
            Message::user("[read_file] ok\n# luu"),
            Message::assistant("It is luu."),
        ];
        let wire = messages(&history, Arguments::String);
        let roles: Vec<&str> = wire.iter().map(|m| m.role).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "assistant"]);
        assert_eq!(wire[2].content, "Sure.");
        assert_eq!(wire[2].tool_calls[0].function.name, "read_file");
        assert_eq!(
            wire[2].tool_calls[0].function.arguments,
            serde_json::Value::String("{\"path\":\"README.md\"}".into())
        );
        assert_eq!(wire[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(wire[3].content, "[read_file] ok\n# luu");

        let object = messages(&history, Arguments::Object);
        assert_eq!(
            object[2].tool_calls[0].function.arguments,
            serde_json::json!({"path": "README.md"})
        );
    }

    #[test]
    fn a_call_without_its_result_after_it_stays_text() {
        // The last step of a turn cut short, or a final answer that happens to
        // quote a call: no `[name]` result follows, so there is nothing to pair.
        let history = vec![
            Message::assistant("```tool\n{\"name\": \"list_dir\", \"arguments\": {}}\n```"),
            Message::user("and now something else"),
        ];
        let wire = messages(&history, Arguments::String);
        assert!(wire.iter().all(|m| m.tool_calls.is_empty()));
        assert_eq!(wire[1].role, "user");
    }

    #[test]
    fn streamed_pieces_assemble_into_calls() {
        let mut calls = Calls::default();
        calls.push(0, Some("read_file"), Some("{\"pa"));
        calls.push(0, None, Some("th\": \"a.rs\"}"));
        let done = calls.finish();
        assert_eq!(done[0].name, "read_file");
        assert_eq!(done[0].arguments, serde_json::json!({"path": "a.rs"}));
        assert_eq!(
            fence(&done[0], true),
            "\n\n```tool\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}\n```"
        );
    }
}
