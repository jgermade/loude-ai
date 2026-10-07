//! Who may reach the control surface.
//!
//! `/ws` carries `approve_task`, `reject_task`, `close_task` and
//! `reopen_task` — approval is the authority this design is built around, so a
//! server that binds off loopback and asks nobody hands that authority to the
//! network. The design doc promised a bearer token there and nothing
//! implemented it; this is that check, and it runs *before* the listener
//! exists so an unauthenticated non-loopback server is not a state the program
//! can be in.
//!
//! See `RECORD/2026-09-01.what-the-audit-left.completed.md`.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Result, bail};

use crate::provider::Exposure;

/// What a bound port requires of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// Loopback with no token configured: the check is the operating system's,
    /// and it is the same one every `luu chat` has always relied on.
    Loopback,
    /// A bearer token, required on the control and read surfaces.
    Token(String),
}

impl Auth {
    /// Whether this request carries what the port requires.
    ///
    /// `presented` is what the request offered, if anything.
    pub fn admits(&self, presented: Option<&str>) -> bool {
        match self {
            Auth::Loopback => true,
            Auth::Token(expected) => {
                presented.is_some_and(|token| constant_time_eq(expected, token))
            }
        }
    }

    pub fn is_token(&self) -> bool {
        matches!(self, Auth::Token(_))
    }
}

/// Decides what the port about to be bound requires, or refuses to bind it.
///
/// Refuse rather than warn, for the reason `luu.toml` ships strict: a warning
/// printed above a served port is a default nobody chose.
///
/// `exposure` is `[server] exposure` (or `--exposure`) when something said it.
/// Absent, the address decides, as it always has. Present, it is a promise
/// the address and the token have to keep, and a run that breaks it is
/// refused rather than read generously: `loopback` with a public address is
/// the mistake this whole function exists to catch.
pub fn resolve(
    address: &SocketAddr,
    token_file: Option<&Path>,
    exposure: Option<Exposure>,
) -> Result<Auth> {
    match exposure {
        Some(Exposure::Loopback) if !address.ip().is_loopback() => bail!(
            "refusing to serve {address}: exposure = \"loopback\" says this machine only, and \
             {address} is not loopback. Bind 127.0.0.1, or say exposure = \"private\" with a \
             token."
        ),
        Some(Exposure::Loopback) if token_file.is_some() => bail!(
            "exposure = \"loopback\" asks nobody for anything, and a token file was named. \
             Say exposure = \"private\" to require it, or drop the token."
        ),
        Some(Exposure::Private) if token_file.is_none() => bail!(
            "exposure = \"private\" requires a bearer token, and none was named. Make one with \
             `luu token <PATH>`, and name it as [server] token-file or --auth-token-file."
        ),
        _ => {}
    }
    match (token_file, address.ip().is_loopback()) {
        (Some(path), _) => Ok(Auth::Token(read_token(path)?)),
        (None, true) => Ok(Auth::Loopback),
        (None, false) => bail!(
            "refusing to serve {address}: /ws carries task approval, and off loopback that is \
             one request away from anyone who can reach this port. Pass \
             --auth-token-file <PATH> to require a bearer token, or bind a loopback address."
        ),
    }
}

/// The token, from a file whose mode says only its owner can read it.
///
/// The reading and the mode check are [`crate::secret`]'s, shared with the API
/// key a provider carries: same class of secret, and one of them used to be
/// checked while the other was not.
fn read_token(path: &Path) -> Result<String> {
    crate::secret::read(
        path,
        "the auth token file",
        "makes the token as public as the port it guards",
    )
}

/// Compares without leaking where the two differ.
///
/// The tokens are read off the network one guess at a time; an early return on
/// the first wrong byte is what turns that into a byte-by-byte search.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // The length is not a secret — it is visible in every request that carries
    // the token — so comparing it up front only avoids indexing past the end.
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> SocketAddr {
        text.parse().expect("an address")
    }

    #[test]
    fn an_exposure_is_a_promise_the_address_and_the_token_keep() {
        let token = Some(Path::new("/nonexistent/token"));
        let loopback = Some(Exposure::Loopback);
        let private = Some(Exposure::Private);
        // Loopback, said and kept.
        assert_eq!(
            resolve(&address("127.0.0.1:7878"), None, loopback).unwrap(),
            Auth::Loopback
        );
        // Said and broken, either way.
        let public = resolve(&address("0.0.0.0:7878"), None, loopback).unwrap_err();
        assert!(public.to_string().contains("not loopback"), "{public}");
        assert!(resolve(&address("127.0.0.1:7878"), token, loopback).is_err());
        let tokenless = resolve(&address("127.0.0.1:7878"), None, private).unwrap_err();
        assert!(tokenless.to_string().contains("luu token"), "{tokenless}");
    }

    #[test]
    fn loopback_needs_nothing() {
        let auth = resolve(&address("127.0.0.1:7878"), None, None).expect("loopback serves");
        assert_eq!(auth, Auth::Loopback);
        assert!(auth.admits(None));
    }

    #[test]
    fn every_other_address_is_refused_by_name() {
        let error = resolve(&address("0.0.0.0:7878"), None, None).expect_err("refused");
        // The message has to name the flag that fixes it: a refusal a person
        // cannot act on is a bug report about our own CLI.
        assert!(error.to_string().contains("--auth-token-file"), "{error}");
    }

    #[test]
    fn a_token_admits_only_itself() {
        let auth = Auth::Token("s3cret".into());
        assert!(auth.admits(Some("s3cret")));
        assert!(!auth.admits(Some("s3cre")));
        assert!(!auth.admits(Some("s3crets")));
        assert!(!auth.admits(Some("")));
        assert!(!auth.admits(None));
    }

    #[test]
    fn a_token_file_is_read_trimmed_and_checked() {
        let dir = std::env::temp_dir().join(format!("luu-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("token");
        std::fs::write(&path, "  s3cret\n").expect("writing the token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("tightening the mode");
        }

        // Off loopback, with a token: served, and the token is what the file
        // said without the newline an editor added.
        let auth = resolve(&address("0.0.0.0:7878"), Some(&path), None).expect("a token serves");
        assert_eq!(auth, Auth::Token("s3cret".into()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("loosening the mode");
            let error = resolve(&address("0.0.0.0:7878"), Some(&path), None).expect_err("refused");
            assert!(error.to_string().contains("chmod 600"), "{error}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_file_is_not_a_token() {
        let dir = std::env::temp_dir().join(format!("luu-auth-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("token");
        std::fs::write(&path, "\n").expect("writing the file");
        let error = resolve(&address("127.0.0.1:0"), Some(&path), None).expect_err("refused");
        assert!(error.to_string().contains("empty"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
