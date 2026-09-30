//! Any OpenAI-compatible server (`POST /chat/completions`, SSE stream).
//!
//! One implementation, and `llama-server`, vLLM, LM Studio, Ollama's own `/v1`
//! and the hosted endpoints all arrive — which is why this is roadmap item 1:
//! five of six machines in
//! [`ROADMAP/2026-09-01/machines.md`](../../../../ROADMAP/2026-09-01/machines.md)
//! serve through one of those rather than through Ollama.
//!
//! **The rule AGENTS.md prints in bold does not apply here, and assuming it did
//! would be the bug.** For Ollama the window has to be *sent*, as
//! `options.num_ctx`, or the server silently truncates to its own default. This
//! API has no field for the window at all: `max_tokens` caps the *output*, and
//! on `llama-server`, vLLM and LM Studio the window is what the server was
//! started with (`llama-server -c 8192`). So `context_limit` is not sent — not
//! because it does not matter, but because there is nowhere to put it that
//! would not be a different number in a different field.
//!
//! The check therefore moves to the response: `usage.prompt_tokens` comes back
//! on every call and is already compared against our own count per turn. Which
//! is why [`stream_options`] is not optional here — see [`Body`].
//!
//! See `RECORD/2026-09-01.an-openai-compatible-backend.completed.md`.

use futures_util::StreamExt;
use serde::Deserialize;

use super::{
    Backend, BackendError, BackendFuture, Chunk, ChunkStream, CompletionRequest, Constraint,
    StopReason, Usage,
};

/// `llama-server`'s default. Not a claim that it is the likeliest server, just
/// the one this repository's own recipe would reach for first.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080/v1";

pub struct OpenAi {
    base_url: String,
    /// `openai@<host>`, because the record header's whole job is that two runs
    /// are comparable — and "openai" alone cannot tell a 7B on a laptop from a
    /// hosted frontier model.
    name: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl OpenAi {
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let name = format!("openai@{}", host_of(&base_url));
        Self {
            base_url,
            name,
            api_key: None,
            http: reqwest::Client::new(),
        }
    }

    /// The bearer token, when the endpoint wants one.
    ///
    /// Absent means **no header at all**, which is what a local `llama-server`
    /// wants: an empty bearer is worse than nothing, since a server that checks
    /// keys would then reject with a message about the wrong thing.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// What a run should be told once, before it measures anything.
    ///
    /// `None` when there is nothing to warn about. This is the sentence that
    /// keeps the untransferable rule visible: the caller budgeted against a
    /// window this API gives it no way to send, so the two can disagree and the
    /// only place that shows is `usage.prompt_tokens` afterwards.
    pub fn window_caveat(context_limit: Option<u32>) -> Option<String> {
        let limit = context_limit.filter(|limit| *limit > 0)?;
        Some(format!(
            "the window is the server's: --context-limit {limit} is what this run \
             budgets against and there is no field on this API to send it in. Start the \
             server with at least {limit} (llama-server -c {limit}, vLLM \
             --max-model-len {limit}); the prompt_tokens each turn reports is what \
             actually arrived."
        ))
    }
}

impl Default for OpenAi {
    fn default() -> Self {
        Self::new(DEFAULT_BASE_URL)
    }
}

/// `host:port` out of a base URL, for the backend's name. Falls back to the
/// whole string rather than to something invented: a name nobody can trace back
/// to an endpoint is the failure this exists to avoid.
fn host_of(base_url: &str) -> String {
    base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or(base_url)
        .to_string()
}

#[derive(serde::Serialize)]
struct Body<'a> {
    model: &'a str,
    /// The messages as they are under the fenced transport, or converted by
    /// [`super::native::messages`] under the native one.
    messages: serde_json::Value,
    /// Absent under the fenced transport, which is what every request before
    /// it sent. See `RECORD/2026-09-30.native-tool-calls.completed.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    stream: bool,
    /// **Always sent.** Without it these servers stream no usage at all, and a
    /// backend that then reported zero would be claiming the server saw an
    /// empty prompt — the number the budget panel plots against our own count.
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u32>,
    // No `max_tokens`, and no window: see the module note. The output cap that
    // *would* belong here is `--reserve`, which `CompletionRequest` does not
    // carry — one change with one argument, not a field smuggled in beside this.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat<'a>>,
    /// Raw GBNF, undocumented on this endpoint and honoured anyway on a
    /// standalone `llama-server` — silently ignored by Ollama's own `/v1`.
    /// See `RECORD/2026-09-14.the-bare-grammar-field.completed.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    grammar: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// The shape every server that documents `response_format` at all agrees
/// on. `name` is fixed rather than threaded from the caller: one schema, one
/// call, and nothing here reads the name back.
#[derive(serde::Serialize)]
struct ResponseFormat<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: JsonSchema<'a>,
}

#[derive(serde::Serialize)]
struct JsonSchema<'a> {
    name: &'static str,
    schema: &'a serde_json::Value,
}

/// Only the fields we act on, so a server-side addition never breaks the parse.
#[derive(Deserialize)]
struct Event {
    #[serde(default)]
    choices: Vec<Choice>,
    /// Arrives on its own final event, after the last choice, when
    /// `include_usage` was asked for.
    #[serde(default)]
    usage: Option<ApiUsage>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// A call the server parsed out of the model's reply, in pieces: the
    /// first delta for an `index` names it, the ones after carry fragments of
    /// its arguments' JSON string.
    #[serde(default)]
    tool_calls: Vec<DeltaCall>,
}

#[derive(Deserialize)]
struct DeltaCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

/// The shape OpenAI documents, and what `llama-server` sends mid-stream when a
/// request goes wrong after the headers are already out.
#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: Option<String>,
}

/// What one SSE `data:` payload stands for.
///
/// Three outcomes and not two: a payload can carry text, or the stop reason, or
/// the usage that arrives *after* it on its own event — so a parse cannot
/// return a single `Chunk` and be done.
#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Text(String),
    /// Pieces of calls: `(index, name, arguments fragment)`.
    Calls(Vec<(usize, Option<String>, Option<String>)>),
    Stop(StopReason),
    Usage(Usage),
    /// A keep-alive, an empty delta, or the role-only first event.
    Nothing,
}

fn parse_event(payload: &[u8]) -> Result<Parsed, BackendError> {
    let event: Event =
        serde_json::from_slice(payload).map_err(|e| BackendError::Malformed(e.to_string()))?;

    if let Some(error) = event.error {
        return Err(BackendError::Rejected(
            error.message.unwrap_or_else(|| "no message".into()),
        ));
    }

    if let Some(choice) = event.choices.first() {
        // Before the stop reason, because a server may send the last fragment
        // of a call on the same event as `finish_reason`, and the stop is
        // implied by `[DONE]` anyway where it is lost this way.
        if !choice.delta.tool_calls.is_empty() {
            return Ok(Parsed::Calls(
                choice
                    .delta
                    .tool_calls
                    .iter()
                    .map(|call| {
                        let function = call.function.as_ref();
                        (
                            call.index,
                            function.and_then(|f| f.name.clone()),
                            function.and_then(|f| f.arguments.clone()),
                        )
                    })
                    .collect(),
            ));
        }
        if let Some(reason) = &choice.finish_reason {
            return Ok(Parsed::Stop(match reason.as_str() {
                "stop" => StopReason::Stop,
                // A reply that ends in a call the server parsed ended the way
                // a reply with a fenced call does: normally. The call itself
                // is what the loop reads next.
                "tool_calls" => StopReason::Stop,
                "length" => StopReason::Length,
                // `content_filter`, or something this server invented. Reported as itself rather than guessed at.
                _ => StopReason::Other,
            }));
        }
        if let Some(text) = &choice.delta.content
            && !text.is_empty()
        {
            return Ok(Parsed::Text(text.clone()));
        }
    }

    // The usage event carries an empty `choices`, so it is checked after them
    // and not instead: a server that sends both on one event still streams its
    // text.
    if let Some(usage) = event.usage {
        return Ok(Parsed::Usage(Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
        }));
    }

    Ok(Parsed::Nothing)
}

/// The `data:` payload of one SSE line, or `None` for a line that is not one —
/// a blank separator, or a `:` comment some servers send as a heartbeat.
fn payload_of(line: &[u8]) -> Option<&[u8]> {
    let text = line.strip_prefix(b"data:")?;
    let payload = match text.first() {
        Some(b' ') => &text[1..],
        _ => text,
    };
    (!payload.is_empty()).then_some(payload)
}

/// `GET /models` — every OpenAI-compatible server answers this one, including
/// `llama-server`, vLLM and LM Studio, each with the single model it loaded.
#[derive(Deserialize)]
struct Models {
    #[serde(default)]
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
}

impl Backend for OpenAi {
    fn name(&self) -> &str {
        &self.name
    }

    fn models(&self) -> BackendFuture<'_, Vec<String>> {
        let url = format!("{}/models", self.base_url);
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        Box::pin(async move {
            let mut request = http.get(&url).timeout(super::LIST_TIMEOUT);
            if let Some(key) = api_key {
                request = request.bearer_auth(key);
            }
            let response = request
                .send()
                .await
                .map_err(|error| BackendError::Transport(error.to_string()))?;
            if !response.status().is_success() {
                return Err(BackendError::Rejected(format!(
                    "{} from {url}",
                    response.status()
                )));
            }
            let models: Models = response
                .json()
                .await
                .map_err(|error| BackendError::Malformed(error.to_string()))?;
            Ok(models.data.into_iter().map(|model| model.id).collect())
        })
    }

    /// `llama-server`'s `GET /props` (`n_ctx`, the window it was started with),
    /// then the models listing's `max_model_len` (vLLM). A server that answers
    /// neither says nothing, which is `None`. `/props` lives beside `/v1`, not
    /// under it.
    fn window(&self) -> BackendFuture<'_, Option<u32>> {
        let root = self.base_url.trim_end_matches("/v1").to_string();
        let models = format!("{}/models", self.base_url);
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        Box::pin(async move {
            let get = |url: String| {
                let mut request = http.get(url).timeout(super::LIST_TIMEOUT);
                if let Some(key) = &api_key {
                    request = request.bearer_auth(key);
                }
                request
            };
            let json = |response: reqwest::Response| async move {
                match response.status().is_success() {
                    true => response.json::<serde_json::Value>().await.ok(),
                    false => None,
                }
            };
            let n = |value: Option<&serde_json::Value>| {
                value
                    .and_then(serde_json::Value::as_u64)
                    .filter(|n| *n > 0)
                    .and_then(|n| u32::try_from(n).ok())
            };
            if let Ok(response) = get(format!("{root}/props")).send().await
                && let Some(props) = json(response).await
                && let Some(window) = n(props
                    .pointer("/default_generation_settings/n_ctx")
                    .or_else(|| props.get("n_ctx")))
            {
                return Ok(Some(window));
            }
            if let Ok(response) = get(models).send().await
                && let Some(listing) = json(response).await
                && let Some(window) = n(listing.pointer("/data/0/max_model_len"))
            {
                return Ok(Some(window));
            }
            Ok(None)
        })
    }

    /// `response_format` is what the OpenAI-shaped spec itself documents, and
    /// every server behind this backend that claims compatibility at all
    /// tends to agree on it. `grammar` is the opposite: measured, on this
    /// project's own `llama-server`, to be honoured — and measured, on
    /// Ollama's own `/v1`, to be silently ignored (same field, same
    /// endpoint, different server). "OpenAI-compatible" does not imply
    /// grammar support, so this says so every time one is sent, regardless
    /// of which server is actually behind `base_url`. See
    /// `RECORD/2026-09-14.the-bare-grammar-field.completed.md`.
    fn constrain_caveat(&self, constraint: &Constraint) -> Option<String> {
        match constraint {
            Constraint::Grammar(_) => Some(format!(
                "sending `grammar` to {}: `llama-server` itself honours this, undocumented; \
                 Ollama's own /v1 silently ignores it. Whether it did anything here is not \
                 knowable from this side of the request.",
                self.base_url
            )),
            Constraint::Schema(_) => None,
        }
    }

    fn stream(&self, request: CompletionRequest) -> ChunkStream<'_> {
        let url = format!("{}/chat/completions", self.base_url);
        let http = self.http.clone();
        let api_key = self.api_key.clone();

        Box::pin(async_stream::try_stream! {
            let (response_format, grammar) = match &request.constraint {
                Some(Constraint::Schema(schema)) => (
                    Some(ResponseFormat {
                        kind: "json_schema",
                        json_schema: JsonSchema {
                            name: "tool_call",
                            schema,
                        },
                    }),
                    None,
                ),
                Some(Constraint::Grammar(grammar)) => (None, Some(grammar.as_str())),
                None => (None, None),
            };
            let native = !request.tools.is_empty();
            let messages = match native {
                true => serde_json::to_value(super::native::messages(
                    &request.messages,
                    super::native::Arguments::String,
                )),
                false => serde_json::to_value(&request.messages),
            }
            .map_err(|e| BackendError::Malformed(e.to_string()))?;
            let mut post = http.post(&url).json(&Body {
                model: &request.model,
                messages,
                tools: native.then(|| super::native::tools(&request.tools)),
                stream: true,
                stream_options: StreamOptions { include_usage: true },
                temperature: request.temperature,
                seed: request.seed,
                response_format,
                grammar,
            });
            if let Some(key) = &api_key {
                post = post.bearer_auth(key);
            }

            let response = post
                .send()
                .await
                .map_err(|e| BackendError::Transport(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                Err(BackendError::Rejected(format!("{status}: {body}")))?;
                return;
            }

            let mut body = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            // Held rather than yielded on sight: the stop reason arrives on one
            // event and the usage on the next, and `Chunk::Done` carries both.
            // Yielding at the stop would report every run as "not reported".
            let mut stop: Option<StopReason> = None;
            let mut usage: Option<Usage> = None;
            let mut done = false;
            // Calls the server parsed, assembled across deltas and handed up
            // as text once the reply is over — see `super::native`.
            let mut calls = super::native::Calls::default();
            let mut texted = false;

            while let Some(bytes) = body.next().await {
                let bytes = bytes.map_err(|e| BackendError::Transport(e.to_string()))?;
                buf.extend_from_slice(&bytes);

                // A chunk boundary lands anywhere, so only whole lines are parsed.
                while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=nl).collect();
                    let line = match line.strip_suffix(b"\n") {
                        Some(line) => line,
                        None => &line,
                    };
                    let line = match line.strip_suffix(b"\r") {
                        Some(line) => line,
                        None => line,
                    };
                    let Some(payload) = payload_of(line) else { continue };
                    if payload == b"[DONE]" {
                        done = true;
                        continue;
                    }
                    match parse_event(payload)? {
                        Parsed::Text(text) => {
                            texted = true;
                            yield Chunk::Text(text)
                        }
                        Parsed::Calls(pieces) => {
                            for (index, name, arguments) in pieces {
                                calls.push(index, name.as_deref(), arguments.as_deref());
                            }
                        }
                        Parsed::Stop(reason) => stop = Some(reason),
                        Parsed::Usage(reported) => usage = Some(reported),
                        Parsed::Nothing => {}
                    }
                }
            }

            // A stream that ended without `[DONE]` and without a stop reason was
            // cut off, and saying it stopped normally would be inventing the one
            // fact the caller reads to decide whether the answer is complete.
            let stop = match (stop, done) {
                (Some(stop), _) => stop,
                (None, true) => StopReason::Stop,
                (None, false) => {
                    Err(BackendError::Transport(
                        "the stream ended without a finish_reason and without [DONE]".into(),
                    ))?;
                    return;
                }
            };
            for call in calls.finish() {
                yield Chunk::Text(super::native::fence(&call, texted));
                texted = true;
            }
            yield Chunk::Done { stop, usage };
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Message;

    #[test]
    fn a_text_delta_is_text_and_an_empty_one_is_nothing() {
        let event = br#"{"choices":[{"delta":{"content":"Hola"}}]}"#;
        assert_eq!(parse_event(event).unwrap(), Parsed::Text("Hola".into()));

        // The first event of every stream: a role and no content.
        let role = br#"{"choices":[{"delta":{"role":"assistant"}}]}"#;
        assert_eq!(parse_event(role).unwrap(), Parsed::Nothing);
    }

    #[test]
    fn the_finish_reason_becomes_a_stop_reason_and_the_unknown_ones_say_so() {
        let stop = br#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        assert_eq!(parse_event(stop).unwrap(), Parsed::Stop(StopReason::Stop));

        let length = br#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#;
        assert_eq!(
            parse_event(length).unwrap(),
            Parsed::Stop(StopReason::Length)
        );

        // A `tool_calls` finish was `Other` while nothing read the call: then
        // it was a reply whose point had been lost, and `Stop` would have hidden
        // that. Since the native transport the call comes up as text — see
        // `a_call_in_pieces_is_read` — so it ended the way a fenced call does.
        let tools = br#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(parse_event(tools).unwrap(), Parsed::Stop(StopReason::Stop));

        let filtered = br#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#;
        assert_eq!(
            parse_event(filtered).unwrap(),
            Parsed::Stop(StopReason::Other)
        );
    }

    /// The OpenAI API's shape for a call the server parsed: named on the first
    /// delta for its index, arguments in fragments after — and the last
    /// fragment may share its event with the finish.
    #[test]
    fn a_call_in_pieces_is_read() {
        let first = br#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#;
        let last = br#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(
            parse_event(first).unwrap(),
            Parsed::Calls(vec![(0, Some("read_file".into()), Some(String::new()))])
        );
        assert_eq!(
            parse_event(last).unwrap(),
            Parsed::Calls(vec![(0, None, Some("{\"path\":\"a\"}".into()))])
        );
    }

    #[test]
    fn the_usage_event_carries_no_choices_and_is_still_read() {
        let event = br#"{"choices":[],"usage":{"prompt_tokens":26,"completion_tokens":7}}"#;
        assert_eq!(
            parse_event(event).unwrap(),
            Parsed::Usage(Usage {
                prompt_tokens: 26,
                completion_tokens: 7,
            })
        );
    }

    #[test]
    fn an_error_object_mid_stream_is_a_rejection() {
        let event = br#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#;
        assert!(matches!(
            parse_event(event),
            Err(BackendError::Rejected(message)) if message.contains("model not found")
        ));
    }

    #[test]
    fn only_data_lines_carry_a_payload() {
        assert_eq!(payload_of(b"data: {\"a\":1}"), Some(&b"{\"a\":1}"[..]));
        // Some servers omit the space; the spec allows it either way.
        assert_eq!(payload_of(b"data:{\"a\":1}"), Some(&b"{\"a\":1}"[..]));
        // A heartbeat comment and a blank separator are not events.
        assert_eq!(payload_of(b": ping"), None);
        assert_eq!(payload_of(b""), None);
        assert_eq!(payload_of(b"event: message"), None);
        assert_eq!(payload_of(b"data:"), None);
    }

    /// The window is not on the request, and that is the decision rather than
    /// an omission: this API has no field for it.
    #[test]
    fn the_body_carries_sampling_and_asks_for_usage_and_sends_no_window() {
        let messages = [Message::user("hola")];
        let body = serde_json::to_value(Body {
            model: "qwen2.5-coder-7b",
            messages: serde_json::to_value(messages).unwrap(),
            tools: None,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            temperature: Some(0.0),
            seed: Some(42),
            response_format: None,
            grammar: None,
        })
        .unwrap();

        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["seed"], 42);
        assert!(body.get("max_tokens").is_none(), "{body}");
        assert!(body.get("num_ctx").is_none(), "{body}");
        assert!(body.get("context_limit").is_none(), "{body}");
    }

    #[test]
    fn unpinned_sampling_is_left_to_the_server() {
        let messages = [Message::user("hola")];
        let body = serde_json::to_value(Body {
            model: "qwen2.5-coder-7b",
            messages: serde_json::to_value(messages).unwrap(),
            tools: None,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            temperature: None,
            seed: None,
            response_format: None,
            grammar: None,
        })
        .unwrap();
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("seed").is_none(), "{body}");
    }

    /// Two runs against different endpoints must not both be called "openai" in
    /// a record header whose whole job is that runs are comparable.
    #[test]
    fn the_name_says_which_endpoint() {
        assert_eq!(
            OpenAi::new("http://127.0.0.1:8080/v1").name(),
            "openai@127.0.0.1:8080"
        );
        assert_eq!(
            OpenAi::new("https://api.example.com/v1").name(),
            "openai@api.example.com"
        );
    }

    #[test]
    fn the_caveat_names_the_window_it_cannot_send() {
        let caveat = OpenAi::window_caveat(Some(8192)).expect("a budgeted run says something");
        assert!(caveat.contains("8192"), "{caveat}");
        assert!(caveat.contains("llama-server -c 8192"), "{caveat}");
        // Nothing to warn about when nothing was budgeted.
        assert_eq!(OpenAi::window_caveat(None), None);
        assert_eq!(OpenAi::window_caveat(Some(0)), None);
    }
}
