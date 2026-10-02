//! Another machine's `luu serve`, reached through this one.
//!
//! The page asks for `/h/<name>/api/…` and `/h/<name>/ws…`, and this module
//! forwards it to the URL `[host.<name>]` names in `config.toml`, with that
//! host's token in the header. **This server keeps nothing**: the session is
//! the host's, made and recorded there, which is what *a session stays on the
//! host that made it* asks. What this adds is a browser that can reach a
//! second server without a second tab and a pasted token.
//!
//! What bounds it:
//!
//! - **A name, never a URL.** The browser names a host; the URL is what
//!   somebody wrote on this machine.
//! - **The token is a file**, read by [`crate::secret::read`] with its mode
//!   checked, and never sent to the page.
//! - **This server's own `Authorization` and `?token=` are dropped** before
//!   forwarding: this server's token is not the host's, and is none of its
//!   business.
//! - **Loopback only**, which `serve` decides: see `hosts_allowed` there.
//!
//! See `RECORD/2026-10-02.a-project-is-a-host-a-folder-and-a-session.completed.md`.

use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message as WsMessage, WebSocket};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

use crate::provider::Host;

/// How long a host may take to accept a connection. Not a deadline on the
/// answer: `/api/workspace/events` is a stream that never ends, and a model
/// list can take as long as the provider behind it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Request headers that are about *this* hop, or this server's own
/// credential, and never forwarded.
const NOT_FORWARDED: &[header::HeaderName] = &[
    header::HOST,
    header::AUTHORIZATION,
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::CONTENT_LENGTH,
    header::UPGRADE,
    header::ORIGIN,
    header::REFERER,
    header::COOKIE,
];

/// Response headers that are about the other hop.
const NOT_RETURNED: &[header::HeaderName] = &[
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::CONTENT_LENGTH,
];

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // A host is a name somebody wrote down; a redirect would be the
            // host naming somewhere else, with the token attached.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client with no TLS configuration to fail")
    })
}

/// The host's token, when it names a file. Read on every request rather than
/// once, so replacing the file is enough to rotate it.
fn token(host: &Host) -> Result<Option<String>, String> {
    let Some(path) = &host.token_file else {
        return Ok(None);
    };
    let path = crate::provider::expand_home(path);
    crate::secret::read(
        &path,
        "the host's token file",
        "hands that host's approvals to anyone who can read it",
    )
    .map(Some)
    .map_err(|error| format!("{error:#}"))
}

/// `rest` with this server's `token` taken out of its query, if it had one.
fn without_token(rest: &str) -> String {
    let Some((path, query)) = rest.split_once('?') else {
        return rest.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some("token"))
        .collect();
    match kept.is_empty() {
        true => path.to_string(),
        false => format!("{path}?{}", kept.join("&")),
    }
}

/// The URL on the host for `rest`, which starts with `/`.
fn target(host: &Host, rest: &str) -> String {
    format!("{}{}", host.url.trim_end_matches('/'), without_token(rest))
}

/// An error and what caused it, down to the bottom: reqwest's own message is
/// "error sending request", and "Connection refused" is two sources below it.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let said = cause.to_string();
        if !text.contains(&said) {
            text = format!("{text}: {said}");
        }
        source = cause.source();
    }
    text
}

fn bad_gateway(name: &str, host: &Host, why: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        format!("host {name} ({}): {why}", host.url),
    )
        .into_response()
}

/// One HTTP request, forwarded, and its answer streamed back as it comes —
/// `events` is a stream that does not end, and an upload is not buffered.
pub async fn forward(
    name: &str,
    host: &Host,
    method: Method,
    rest: &str,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let token = match token(host) {
        Ok(token) => token,
        Err(why) => return bad_gateway(name, host, why),
    };
    let mut asked = client().request(method, target(host, rest));
    for (key, value) in headers {
        if !NOT_FORWARDED.contains(key) {
            asked = asked.header(key, value);
        }
    }
    if let Some(token) = token {
        asked = asked.bearer_auth(token);
    }
    asked = asked.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    let answer = match asked.send().await {
        Ok(answer) => answer,
        Err(error) => return bad_gateway(name, host, chain(&error)),
    };
    let mut response = Response::builder().status(answer.status().as_u16());
    for (key, value) in answer.headers() {
        if !NOT_RETURNED.contains(key) {
            response = response.header(key.as_str(), value.as_bytes());
        }
    }
    response
        .body(Body::from_stream(answer.bytes_stream()))
        .unwrap_or_else(|error| bad_gateway(name, host, error))
}

/// Opens the host's end of a socket, before this end is upgraded, so a host
/// that refuses is a status and a sentence rather than a socket that opens and
/// closes at once.
pub async fn connect(name: &str, host: &Host, rest: &str) -> Result<Upstream, Box<Response>> {
    let refuse = |why: String| Box::new(bad_gateway(name, host, why));
    let token = token(host).map_err(refuse)?;
    let url = target(host, rest);
    let ws = match url.split_once("://") {
        Some(("http", tail)) => format!("ws://{tail}"),
        Some(("https", tail)) => format!("wss://{tail}"),
        _ => return Err(refuse(format!("{url} is not http:// or https://"))),
    };
    let mut request = ws
        .as_str()
        .into_client_request()
        .map_err(|error| refuse(error.to_string()))?;
    // The host's terminal admits only a socket whose `Origin` is the host
    // itself — its own page. Through here, that page is this server's, which
    // `serve` has already checked is same-origin with *this* server.
    let origin = host.url.trim_end_matches('/').to_string();
    if let Ok(value) = HeaderValue::from_str(&origin) {
        request.headers_mut().insert(header::ORIGIN, value);
    }
    if let Some(token) = token
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        request.headers_mut().insert(header::AUTHORIZATION, value);
    }
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
        Ok(Ok((stream, _))) => Ok(Upstream(stream)),
        Ok(Err(tungstenite::Error::Http(answer))) => {
            let status = answer.status();
            let body = answer
                .body()
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default()
                .trim()
                .to_string();
            Err(Box::new(
                (
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                    format!("host {name}: {body}"),
                )
                    .into_response(),
            ))
        }
        Ok(Err(error)) => Err(refuse(chain(&error))),
        Err(_) => Err(refuse("no answer within 5 s".to_string())),
    }
}

/// The host's end of a socket.
pub struct Upstream(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
);

/// Carries frames both ways until either end closes.
pub async fn pipe(socket: WebSocket, upstream: Upstream) {
    let (mut to_page, mut from_page) = socket.split();
    let (mut to_host, mut from_host) = upstream.0.split();
    let up = async {
        while let Some(Ok(message)) = from_page.next().await {
            let message = match message {
                WsMessage::Text(text) => tungstenite::Message::text(text.as_str()),
                WsMessage::Binary(bytes) => tungstenite::Message::binary(bytes),
                WsMessage::Ping(bytes) => tungstenite::Message::Ping(bytes),
                WsMessage::Pong(bytes) => tungstenite::Message::Pong(bytes),
                WsMessage::Close(_) => break,
            };
            if to_host.send(message).await.is_err() {
                break;
            }
        }
        let _ = to_host.close().await;
    };
    let down = async {
        while let Some(Ok(message)) = from_host.next().await {
            let message = match message {
                tungstenite::Message::Text(text) => WsMessage::Text(text.as_str().into()),
                tungstenite::Message::Binary(bytes) => WsMessage::Binary(bytes),
                tungstenite::Message::Ping(bytes) => WsMessage::Ping(bytes),
                tungstenite::Message::Pong(bytes) => WsMessage::Pong(bytes),
                tungstenite::Message::Close(frame) => {
                    let _ = to_page
                        .send(WsMessage::Close(frame.map(|frame| CloseFrame {
                            code: frame.code.into(),
                            reason: frame.reason.as_str().into(),
                        })))
                        .await;
                    break;
                }
                tungstenite::Message::Frame(_) => continue,
            };
            if to_page.send(message).await.is_err() {
                break;
            }
        }
    };
    // Either side ending ends both: a page that left has nobody to read the
    // host, and a host that left has nothing to say.
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_servers_token_does_not_travel() {
        assert_eq!(without_token("/ws?token=abc"), "/ws");
        assert_eq!(
            without_token("/api/workspace/tree?path=a&token=abc"),
            "/api/workspace/tree?path=a"
        );
        assert_eq!(
            without_token("/api/workspace/tree?path=a"),
            "/api/workspace/tree?path=a"
        );
        assert_eq!(without_token("/api/settings"), "/api/settings");
    }

    #[test]
    fn the_url_is_the_hosts_and_the_path_is_the_pages() {
        let host = Host {
            url: "http://ryzen.local:7878/".into(),
            token_file: None,
        };
        assert_eq!(
            target(&host, "/api/settings?token=x"),
            "http://ryzen.local:7878/api/settings"
        );
    }
}
