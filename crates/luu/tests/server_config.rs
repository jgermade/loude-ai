//! `[server]` in `config.toml`, and the `LUU_*` variables over it, deciding
//! how a real `luu serve` is reached. The binary, for `tests/hosts.rs`'s
//! reason: the file is read from `LUU_HOME`, and in-process that would be
//! every other test's environment too. See
//! `RECORD/2026-10-07.a-public-luu.WIP.md`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

fn scratch() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("luu-server-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn luu(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_luu"));
    command
        .current_dir(root())
        .env("LUU_HOME", home)
        .env_remove("LUU_BIND")
        .env_remove("LUU_EXPOSURE")
        .env_remove("LUU_AUTH_TOKEN_FILE")
        .env_remove("LUU_ORIGIN");
    command
}

/// Starts `serve`, and answers the address it printed, or what it said on
/// stderr when it refused to start.
fn start(mut command: Command) -> Result<(std::process::Child, String), String> {
    let mut child = command
        .args(["serve", "--no-store"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting luu serve");
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("luu serve has no stdout");
    };
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            let mut said = String::new();
            let _ = std::io::Read::read_to_string(
                child.stderr.as_mut().expect("its stderr"),
                &mut said,
            );
            let _ = child.wait();
            return Err(said);
        }
        if let Some(address) = line.trim().strip_prefix("luu serve → http://") {
            let address = address.to_string();
            std::thread::spawn(move || for _ in reader.lines() {});
            return Ok((child, address));
        }
    }
}

#[tokio::test]
async fn the_file_makes_a_server_private_and_the_environment_cannot_loosen_it() {
    let home = scratch();
    let token = home.join("token");
    let status = luu(&home)
        .args(["token", token.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[server]\nexposure = \"private\"\nbind = \"127.0.0.1:0\"\ntoken-file = \"{}\"\n",
            token.display()
        ),
    )
    .unwrap();

    let (mut child, address) = start(luu(&home)).expect("a private server starts");
    let http = reqwest::Client::new();
    let bare = http
        .get(format!("http://{address}/api/settings"))
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 401, "private asks for the token");
    let secret = std::fs::read_to_string(&token).unwrap();
    let carried = http
        .get(format!("http://{address}/api/settings"))
        .bearer_auth(secret.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(carried.status(), 200);
    let _ = child.kill();
    let _ = child.wait();

    // A variable overrides the file — and a contradiction is refused, not
    // read generously.
    let mut public = luu(&home);
    public
        .env("LUU_EXPOSURE", "loopback")
        .env("LUU_BIND", "0.0.0.0:0");
    let refused = start(public).expect_err("loopback on 0.0.0.0 is refused");
    assert!(refused.contains("not loopback"), "{refused}");
}

#[tokio::test]
async fn private_without_a_token_names_the_command_that_makes_one() {
    let home = scratch();
    std::fs::write(
        home.join("config.toml"),
        "[server]\nexposure = \"private\"\nbind = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    let refused = start(luu(&home)).expect_err("private with no token is refused");
    assert!(refused.contains("luu token"), "{refused}");
}

#[test]
fn an_origin_that_is_not_one_is_a_load_error() {
    let home = scratch();
    std::fs::write(
        home.join("config.toml"),
        "[server]\norigin = \"https://luu.example.com/app\"\n",
    )
    .unwrap();
    let parsed = luu::provider::Config::from_toml(
        &std::fs::read_to_string(home.join("config.toml")).unwrap(),
        "config.toml",
    );
    assert!(
        matches!(parsed, Err(luu::provider::ConfigError::BadServer { .. })),
        "a path is not part of an origin"
    );
}
