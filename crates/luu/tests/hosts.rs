//! A host, reached through this server: two real `luu serve` processes, the
//! far one guarded by a token and the near one naming it in `config.toml`.
//!
//! The binary and not the router in-process, because what is being tested is
//! the hop — a request leaving one server and arriving at another with the
//! token on it — and because the near one reads `config.toml` from `LUU_HOME`,
//! which in-process would be every other test's environment too. See
//! `RECORD/2026-10-02.a-project-is-a-host-a-folder-and-a-session.completed.md`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

fn scratch() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("luu-hosts-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A server that is killed with the test, however the test ends.
struct Serving(Child, String);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `luu serve` on an ephemeral port, with a state directory of its own, and
/// the address it printed.
fn serve(home: &Path, extra: &[&str]) -> Serving {
    let mut child = Command::new(env!("CARGO_BIN_EXE_luu"))
        .current_dir(root())
        .env("LUU_HOME", home)
        .args(["serve", "--bind", "127.0.0.1:0", "--no-store"])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting luu serve");
    let mut line = String::new();
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("luu serve has no stdout");
    };
    let mut reader = BufReader::new(stdout);
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("luu serve exited before saying where it listens");
        }
        if let Some(address) = line.trim().strip_prefix("luu serve → http://") {
            let address = address.to_string();
            // Kept open, or the server's next `println!` meets a closed pipe.
            std::thread::spawn(move || for _ in reader.lines() {});
            return Serving(child, address);
        }
    }
}

fn owner_only(path: &Path, text: &str) {
    std::fs::write(path, text).expect("writing it");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("tightening the mode");
    }
}

/// The far server, guarded, and the near one naming it `far`.
fn pair() -> (Serving, Serving) {
    let dir = scratch();
    let token = dir.join("far.token");
    owner_only(&token, "s3cret");
    std::fs::create_dir_all(dir.join("far-home")).unwrap();
    let far = serve(
        &dir.join("far-home"),
        &["--auth-token-file", token.to_str().unwrap()],
    );
    std::fs::create_dir_all(dir.join("near-home")).unwrap();
    std::fs::write(
        dir.join("near-home/config.toml"),
        format!(
            "[host.far]\nurl = \"http://{}\"\ntoken-file = \"{}\"\n",
            far.1,
            token.display()
        ),
    )
    .unwrap();
    let near = serve(&dir.join("near-home"), &[]);
    (far, near)
}

#[tokio::test]
async fn a_host_answers_through_this_server_with_its_own_token() {
    let (far, near) = pair();
    let http = reqwest::Client::new();

    // The far one refuses a request with no token: the proxy is what adds it.
    let bare = http
        .get(format!("http://{}/api/settings", far.1))
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 401);

    let through = http
        .get(format!("http://{}/h/far/api/settings", near.1))
        .send()
        .await
        .unwrap();
    assert_eq!(through.status(), 200, "{:?}", through.text().await);

    // The list says which hosts there are, without their tokens.
    let hosts: serde_json::Value = http
        .get(format!("http://{}/api/hosts", near.1))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hosts["hosts"]["far"]["url"], format!("http://{}", far.1));
    assert_eq!(hosts["editable"], true);

    // A name nobody wrote down is not a host.
    let nobody = http
        .get(format!("http://{}/h/nobody/api/settings", near.1))
        .send()
        .await
        .unwrap();
    assert_eq!(nobody.status(), 404);

    // The page under a host's prefix is this server's own.
    let page = http
        .get(format!("http://{}/h/far/", near.1))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("importmap"));
    let asset = http
        .get(format!("http://{}/h/far/app.css", near.1))
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), 200);

    // A write from another site's page is refused before it reaches the host.
    let foreign = http
        .post(format!("http://{}/h/far/api/sessions", near.1))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 403);
}

#[tokio::test]
async fn a_hosts_socket_is_carried_both_ways_and_only_for_this_page() {
    let (_far, near) = pair();
    let url = format!("ws://{}/h/far/ws", near.1);

    // No Origin: refused before anything is forwarded.
    let refused = tokio_tungstenite::connect_async(url.as_str()).await;
    assert!(refused.is_err(), "a socket with no Origin was opened");

    let mut request = url.as_str().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("origin", format!("http://{}", near.1).parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the socket, forwarded");
    // The far server speaks first on `/ws`, which proves the frames come back;
    // a guarded port answers a prompt sent before the client's own hello
    // with a refusal, which proves they go out — and that the far server
    // treated the socket as one from off its machine.
    let hello = tokio::time::timeout(std::time::Duration::from_secs(20), socket.next())
        .await
        .expect("a message in time")
        .expect("a message")
        .expect("not an error");
    let hello: serde_json::Value =
        serde_json::from_str(hello.to_text().expect("text")).expect("json");
    assert_eq!(hello["type"], "hello");
    socket
        .send(tokio_tungstenite::tungstenite::Message::text(
            r#"{"type":"prompt","text":"hola"}"#,
        ))
        .await
        .unwrap();
    let message = tokio::time::timeout(std::time::Duration::from_secs(20), socket.next())
        .await
        .expect("an answer in time")
        .expect("an answer")
        .expect("not an error");
    let answer: serde_json::Value =
        serde_json::from_str(message.to_text().expect("text")).expect("json");
    assert_eq!(answer["type"], "refused", "{answer}");
    assert_eq!(answer["reason"], "version", "{answer}");
}
