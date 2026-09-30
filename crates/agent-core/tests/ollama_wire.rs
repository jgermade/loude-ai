//! What `Ollama::stream` actually puts on the socket.
//!
//! The unit tests beside the backend assert that a `ChatRequest` *value*
//! serializes the way we mean — and the value under test is one the test built.
//! Nothing there asserts that `stream()` builds that value from the
//! `CompletionRequest` it was handed, which is precisely the bug that shipped:
//! the window was budgeted, never sent, and Ollama truncated the prompt to its
//! own default in silence. This test reads the bytes off a socket instead.
//!
//! The stub is not a general HTTP server. It serves exactly one request, and it
//! knows what that request will be.

use std::net::SocketAddr;

use agent_core::backend::{Backend, Chunk, CompletionRequest, Message, StopReason, ollama::Ollama};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const NDJSON: &str = concat!(
    r#"{"message":{"role":"assistant","content":"Hola "},"done":false}"#,
    "\n",
    r#"{"message":{"role":"assistant","content":"mundo"},"done":false}"#,
    "\n",
    r#"{"done":true,"done_reason":"stop","prompt_eval_count":26,"eval_count":7}"#,
    "\n",
);

/// Binds an ephemeral port — a fixed one is a test that fails whenever two jobs
/// share a runner — and answers the first request with `NDJSON`, handing back
/// the body it was sent.
async fn stub() -> (SocketAddr, JoinHandle<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binding");
    let address = listener.local_addr().expect("the bound address");

    let served = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("a connection");
        let mut raw: Vec<u8> = Vec::new();

        // Headers first, then exactly as many body bytes as were announced.
        // reqwest sends a `Content-Length` for a JSON body, so there is no
        // chunked case to handle here.
        let head = loop {
            let mut buf = [0u8; 1024];
            let read = socket.read(&mut buf).await.expect("reading the request");
            assert!(read > 0, "the client closed before sending a request");
            raw.extend_from_slice(&buf[..read]);
            if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
        };

        let headers = String::from_utf8_lossy(&raw[..head]).to_lowercase();
        let length: usize = headers
            .split("content-length:")
            .nth(1)
            .and_then(|rest| rest.split("\r\n").next())
            .map(|value| value.trim().parse().expect("a numeric content-length"))
            .expect("a content-length on the request");

        while raw.len() < head + length {
            let mut buf = [0u8; 1024];
            let read = socket.read(&mut buf).await.expect("reading the body");
            assert!(read > 0, "the body ended early");
            raw.extend_from_slice(&buf[..read]);
        }

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\n\r\n{}",
            NDJSON.len(),
            NDJSON,
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("writing the response");
        socket.shutdown().await.expect("closing");

        serde_json::from_slice(&raw[head..head + length]).expect("a JSON request body")
    });

    (address, served)
}

/// Drives one request through the real backend and returns the body the server
/// saw, together with the chunks the client got back.
async fn round_trip(request: CompletionRequest) -> (serde_json::Value, Vec<Chunk>) {
    let (address, served) = stub().await;
    let backend = Ollama::new(format!("http://{address}"));

    let mut chunks = Vec::new();
    let mut stream = backend.stream(request);
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk.expect("a chunk"));
    }

    (served.await.expect("the stub server"), chunks)
}

fn request() -> CompletionRequest {
    CompletionRequest {
        tools: Vec::new(),
        model: "qwen2.5-coder:7b".into(),
        messages: vec![Message::system("you are luu"), Message::user("hola")],
        context_limit: None,
        temperature: None,
        seed: None,
        constraint: None,
    }
}

#[tokio::test]
async fn the_window_and_the_pinned_sampling_reach_the_server() {
    let (body, _) = round_trip(CompletionRequest {
        context_limit: Some(8192),
        temperature: Some(0.0),
        seed: Some(42),
        ..request()
    })
    .await;

    assert_eq!(body["options"]["num_ctx"], 8192);
    assert_eq!(body["options"]["temperature"], 0.0);
    assert_eq!(body["options"]["seed"], 42);
}

#[tokio::test]
async fn the_messages_arrive_in_order_and_the_stream_is_asked_for() {
    let (body, _) = round_trip(request()).await;

    assert_eq!(body["model"], "qwen2.5-coder:7b");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "you are luu");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "hola");
}

/// An unknown window is not a window of zero and not a default we invented: the
/// key is absent and the server keeps its own.
#[tokio::test]
async fn an_unknown_window_and_unpinned_sampling_send_no_options_at_all() {
    let (body, _) = round_trip(request()).await;

    assert!(body.get("options").is_none(), "{body}");
}

/// The response half, free once the server exists: the NDJSON has to come back
/// as the chunks the turn loop consumes, usage included.
#[tokio::test]
async fn the_ndjson_comes_back_as_text_chunks_and_a_done() {
    let (_, chunks) = round_trip(request()).await;

    let text: String = chunks
        .iter()
        .filter_map(|chunk| match chunk {
            Chunk::Text(text) => Some(text.as_str()),
            Chunk::Done { .. } => None,
        })
        .collect();
    assert_eq!(text, "Hola mundo");

    // `Some`, not merely non-zero: Ollama's done line always carries the
    // counts, and a run of this backend that reported "not reported" would be
    // a change worth failing on.
    let Some(Chunk::Done {
        stop,
        usage: Some(usage),
    }) = chunks.last()
    else {
        panic!("the stream ended without a Done carrying usage: {chunks:?}");
    };
    assert_eq!(*stop, StopReason::Stop);
    assert_eq!(usage.prompt_tokens, 26);
    assert_eq!(usage.completion_tokens, 7);
}
