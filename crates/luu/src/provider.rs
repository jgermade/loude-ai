//! Where a model lives, written down once.
//!
//! A provider is a name for a destination — which backend, at which URL, with
//! which model and which key — kept in `config.toml` **in the state directory**
//! rather than in `luu.toml`. The split is the store's: `luu.toml` describes
//! *this project* and is committed with it, while a LAN address, a path to a key
//! and a model somebody pulled onto one disk are facts about *one machine*.
//!
//! ```toml
//! default = "local"
//!
//! [provider.local]
//! backend = "ollama"
//! url = "http://127.0.0.1:11434"
//! model = "qwen2.5-coder:7b"
//!
//! [provider.workstation]
//! backend = "openai"
//! url = "http://192.168.1.40:8080/v1"
//! model = "qwen2.5-coder-14b"
//! context-limit = 16384
//! ```
//!
//! **The one rule that is not bookkeeping**: the profile `default` names must
//! declare `remote = true` when its URL is not this machine, and must not
//! declare it when it is. `local-first` promises that *nothing leaves the
//! machine without the destination having been typed*, and a profile is a
//! destination typed once, weeks ago — except that `-p workstation` **is**
//! typing it, at the moment of use. The default is the only shape where nothing
//! was typed, so it is the only one that has to have been written down. A
//! `config.toml` that arrived from somewhere else fails to load if its default
//! points off the machine.
//!
//! See `RECORD/2026-09-07.naming-a-provider.completed.md`, and its later section for
//! why the rule is not on every profile.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The backends a flag or a profile can name.
///
/// One enum for both, so `--backend openai` and `backend = "openai"` cannot
/// drift into meaning different sets.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Mock,
    Ollama,
    /// Any OpenAI-compatible server: `llama-server`, vLLM, LM Studio, a hosted
    /// endpoint, or Ollama's own `/v1`.
    Openai,
}

impl BackendKind {
    /// Where this backend is when nothing said otherwise. The mock is nowhere,
    /// which is why it is `None` rather than a URL nobody dials.
    fn default_url(self) -> Option<&'static str> {
        match self {
            Self::Mock => None,
            Self::Ollama => Some(DEFAULT_OLLAMA_URL),
            Self::Openai => Some(agent_core::backend::openai::DEFAULT_BASE_URL),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Ollama => "ollama",
            Self::Openai => "openai",
        }
    }
}

pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_MODEL: &str = "qwen2.5-coder:7b";
/// The file, inside whatever `crate::config` resolved.
pub const FILE: &str = "config.toml";

/// One named destination.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The window the *server* was started with, which on the OpenAI API is the
    /// only place it exists: there is no field on a request to send it in. A
    /// fact about the server rather than about the run, so it belongs beside the
    /// URL and not in a flag somebody retypes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u32>,
    /// A path, never the key itself. A key in a config file is a key in every
    /// backup, every screen share and every `cat` in a bug report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    /// Declared, not derived — see the module note. Only load-bearing on the
    /// profile `default` names.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote: bool,
    /// An `[engine.<name>]` luu starts before a session sends here. The URL
    /// stays this profile's own rather than derived from the engine's port, so
    /// the destination a run prints is still one line of the file. See
    /// `RECORD/2026-09-28.a-model-server-luu-starts.completed.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// How a call travels: `fenced` (the default, and every recording made
    /// before this key) or `native`, the server's own `tools` field. A fact
    /// about the server beside the URL, because it is the server's template
    /// that renders the tools and the server that has to accept the field.
    /// See `RECORD/2026-09-30.native-tool-calls.completed.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<ToolCalls>,
}

/// The two tool transports. See [`Profile::tool_calls`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ToolCalls {
    /// Definitions as text in the system message, a call as a ```` ```tool ````
    /// block the model types.
    #[default]
    Fenced,
    /// The API's `tools` field: the server's template renders them with the
    /// tool prompt the model was trained on.
    Native,
}

impl ToolCalls {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fenced => "fenced",
            Self::Native => "native",
        }
    }

    pub fn native(self) -> bool {
        self == Self::Native
    }
}

/// `[engine.<name>]`: a model server luu is allowed to start, as a child of
/// `luu serve` that dies with it. See `crate::engines` and
/// `RECORD/2026-09-28.a-model-server-luu-starts.completed.md`.
///
/// Every key but `kind` is optional, and absent means the kind's own answer:
/// the binary found on this machine, the pinned version, the usual port.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Engine {
    pub kind: EngineKind,
    /// `"managed"` for luu's own copy, a path for that binary, or absent for
    /// the one found on `PATH` and the usual places.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// The release tag of the managed copy. Absent is the one luu pins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Which build of that release: llama.cpp's `vulkan`, `cuda-12.8`,
    /// `rocm-10.0`. Absent is the plain CPU/Metal one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The model a llama.cpp engine loads, as a reference into what this
    /// machine already has — `ollama:qwen2.5-coder:7b`,
    /// `huggingface:<org>/<repo>/<file>.gguf` — which luu turns into `-m
    /// <path>`. A session that picks another moves the engine onto it. See
    /// `crate::models`. Unused by ollama, which serves every model it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Passed after the ones luu adds (`--host`, `--port`): the model, the
    /// context, the GPU layers — the half of the command that is the tuning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Seconds a session waits for it to answer. A guess, and a key so that it
    /// can be tuned: `-hf` downloads the model on first use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_timeout: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    /// llama.cpp's `llama-server`, spoken to as `openai`.
    #[default]
    Llama,
    /// `ollama serve`, spoken to as `ollama`.
    Ollama,
    /// `mlx-serve`, spoken to as `openai`: MLX and safetensors models in
    /// Hugging Face's layout, and GGUF through the llama.cpp it embeds, on
    /// Apple Silicon. Chosen over `mlx_lm.server` by measurement — see the
    /// engines record.
    Mlx,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    provider: BTreeMap<String, Profile>,
    /// `[posture.<name>]`: what a session is allowed to do, by the name of the
    /// policy file that decides it. Beside the providers because it is the same
    /// kind of thing — a name written on this machine, so that what a surface
    /// may choose is bounded by what somebody typed here. See
    /// `RECORD/2026-09-08.a-session-picks-its-executor.completed.md`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    posture: BTreeMap<String, Posture>,
    /// `[ui]`: what the debug page looks like, as opposed to where a run
    /// sends. Here because it is the same kind of thing as the two above —
    /// something written on this machine — and because the one setting in it
    /// names a path that only exists on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ui: Option<Ui>,
    /// `[resend]`: how much of the history a turn pays for again. In this file
    /// and not in `localStorage` for the reason the providers are — it changes
    /// the bytes a run sends, which is a fact about the run and not about the
    /// screen somebody reads from. See
    /// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resend: Option<Resend>,
    /// `[authority]`: what a draft or a plan tells the model about itself, as
    /// opposed to what the sandbox enforces without saying so. Beside `resend`
    /// for the same reason — it changes the bytes a turn sends — and its own
    /// table because it answers a different question. See
    /// `RECORD/2026-09-22.an-authority-a-model-is-told.completed.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authority: Option<AuthorityNotes>,
    /// `[engine.<name>]`: model servers luu may start. See [`Engine`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    engine: BTreeMap<String, Engine>,
}

/// `[ui]`. One setting so far.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Ui {
    /// A VSCode icon theme: either an installed extension's directory or a
    /// theme JSON. Nothing is vendored, so with this unset the tree draws the
    /// page's own two glyphs — see `crate::icons`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_theme: Option<PathBuf>,
}

/// `[resend]`: the three rules a window is rendered under, as this machine's
/// default.
///
/// The prompt is rebuilt whole every turn, so the entire history block is a
/// resend — that is what the table is named after, and every rule in it acts
/// only on something already sent at least once.
///
/// **The keys are the rules as they are today, and that is a decision taken
/// knowingly.** The record settled a different cut — two keys by *content*,
/// `file-read` and `cmd-output`, which would put a selected span and
/// `read_file`'s result under one name because they are the same object — and
/// that cut describes behaviour nothing implements yet: rule A reaches spans
/// only, and tool output is reachable only through rule C, behind rule B's
/// line. Writing those keys now would name a file after a capability that does
/// not exist. So the mechanism lands first under the names the header already
/// uses, and renaming them later is a migration of a file on somebody's
/// machine — the cost of this order, accepted rather than overlooked. See
/// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md` §sixth for
/// the cut, and §eleventh for why it is not here.
///
/// Every key is optional and an absent one means *what the code defaults to*,
/// so a hand-written table may name one rule without stating a position on the
/// other two.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Resend {
    /// What a span already in the window costs the turn that selects it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<agent_core::context::Repeat>,
    /// What an old turn's spans cost once the window no longer fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<agent_core::context::Prune>,
    /// What an old turn's tool output costs once its turn is behind the prune
    /// line. Inert without `prune`, and written down anyway: a reader comparing
    /// two machines needs to know which arm each was set to, not which arm had
    /// an effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<agent_core::context::Results>,
}

/// `[authority]`: what a draft or a plan is told about itself, as this
/// machine's default.
///
/// `Authority::Draft` and `Authority::Plan` (`agent_core::sandbox::Authority`)
/// already narrow what a turn may *do*; nothing narrows what it *reads* about
/// that. Two tables and not one field, because a draft and a plan grant
/// different things and the sentence that fits one does not fit the other —
/// asked and rejected as a single note in the record this type argues for.
/// `Authority::Policy` has no table here: it is the ordinary grant, and there
/// is nothing about it worth telling a model that the tools it already has
/// would not show it.
///
/// Every key of every table is optional, absent meaning *no note* — not *some
/// default text* — the same convention [`Resend`] states for the same reason:
/// a hand-written table may set the wording without setting the position, or
/// set neither and mean nothing here yet.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AuthorityNotes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<AuthorityNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<AuthorityNote>,
}

impl AuthorityNotes {
    /// Neither table set. The page skips writing this out where it holds, so
    /// a session that never touched an authority note does not grow a new
    /// empty field in every `/api/settings` response.
    pub fn is_empty(&self) -> bool {
        self.draft.is_none() && self.plan.is_none()
    }

    /// What a draft turn is actually told, in the wire's shape — `None`
    /// wherever this config's `text` is absent or empty, which is what makes
    /// this the one place the config's *optional* words become the header's
    /// and the render's *this note exists or it does not*.
    pub fn draft_note(&self) -> Option<agent_core::sandbox::AuthorityNote> {
        Self::wire(self.draft.as_ref())
    }

    /// The same, for a plan. See [`Self::draft_note`].
    pub fn plan_note(&self) -> Option<agent_core::sandbox::AuthorityNote> {
        Self::wire(self.plan.as_ref())
    }

    fn wire(note: Option<&AuthorityNote>) -> Option<agent_core::sandbox::AuthorityNote> {
        let text = note?.text.as_ref()?;
        if text.is_empty() {
            return None;
        }
        Some(agent_core::sandbox::AuthorityNote {
            text: text.clone(),
            position: note?.position.unwrap_or_default(),
        })
    }
}

/// One authority's note as this machine's config states it, before it is
/// worth sending: what it says, and where it goes.
///
/// **Not [`agent_core::sandbox::AuthorityNote`]**, on purpose — that one is
/// the wire's shape, `text` required because it exists only once a caller
/// decided to send something. This one is the config file's, where `text`
/// absent is the ordinary and expected case and a required field would make
/// every unconfigured table an error.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AuthorityNote {
    /// The words. Absent (not empty) means nothing is sent — the whole feature
    /// is this field, and the reason it exists is to be edited by hand between
    /// runs rather than compiled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Where it goes, defaulting to `System` when `text` is set. See
    /// [`agent_core::sandbox::NotePosition`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<agent_core::sandbox::NotePosition>,
}

/// One named posture: a policy file, and nothing else.
///
/// The file decides the sandbox **and** the seam — `commands`, `network`,
/// `enforcement`, `limits` and `[worker]` — because the loosening and the
/// container were put in one file on purpose
/// (`RECORD/2026-09-01.the-container-decided.completed.md`). So a posture is a
/// path, and never a runtime and an image named separately: the two would be a
/// second way to say what the file already says.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Posture {
    /// Relative to the directory the server runs in, which is the directory the
    /// policy file's own paths are relative to.
    pub policy: PathBuf,
}

/// The file, parsed and checked.
#[derive(Debug, Clone, Default)]
pub struct Config {
    default: Option<String>,
    providers: BTreeMap<String, Profile>,
    postures: BTreeMap<String, Posture>,
    ui: Option<Ui>,
    resend: Option<Resend>,
    authority: Option<AuthorityNotes>,
    engines: BTreeMap<String, Engine>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path}: default = \"{name}\", and there is no [provider.{name}]")]
    NoSuchDefault { path: String, name: String },
    #[error(
        "{path}: [provider.{name}] is the default and {url} is not this machine, so every run \
         with no -p sends there. Add `remote = true` to say so, or name a local profile as \
         the default."
    )]
    UndeclaredRemoteDefault {
        path: String,
        name: String,
        url: String,
    },
    #[error(
        "{path}: [provider.{name}] says `remote = true` and {url} is this machine. The word is \
         there to be read, and one written where it is not true is one nobody reads."
    )]
    RemoteButLocal {
        path: String,
        name: String,
        url: String,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("rendering the config: {message}")]
    Render { message: String },
    #[error("there is no [provider.{name}] in {path}")]
    NoSuchProfile { path: String, name: String },
    #[error("there is no [posture.{name}] in {path}")]
    NoSuchPosture { path: String, name: String },
    #[error(
        "{path}: [provider.{profile}] names engine = \"{engine}\", and there is no [engine.{engine}]"
    )]
    NoSuchEngine {
        path: String,
        profile: String,
        engine: String,
    },
    #[error("-p {name} was named, and this machine has no {file}")]
    NoConfigFile { name: String, file: String },
}

impl Config {
    /// The config file in the state directory, if there is one.
    ///
    /// Deliberately **not** [`crate::config::dir`]: that one asks the first-run
    /// question and creates the directory, and neither belongs to reading a
    /// file that is usually absent. A machine with no state directory has no
    /// providers, which is the same answer with none of the ceremony.
    pub fn load() -> Result<(Self, Option<PathBuf>), ConfigError> {
        let env = crate::config::Env::from_process();
        let crate::config::Resolution::Decided(dir) =
            crate::config::resolve(&env, |path| path.exists())
        else {
            return Ok((Self::default(), None));
        };
        let path = dir.join(FILE);
        if !path.exists() {
            return Ok((Self::default(), None));
        }
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let config = Self::from_toml(&text, &path.display().to_string())?;
        Ok((config, Some(path)))
    }

    /// Parses and checks, with `path` only for what the errors say.
    pub fn from_toml(text: &str, path: &str) -> Result<Self, ConfigError> {
        let file: File = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_string(),
            source,
        })?;
        let config = Self {
            default: file.default,
            providers: file.provider,
            postures: file.posture,
            ui: file.ui,
            resend: file.resend,
            authority: file.authority,
            engines: file.engine,
        };
        config.check_default(path)?;
        config.check_engines(path)?;
        Ok(config)
    }

    /// A profile's `engine` names a table that exists. Checked at load rather
    /// than at the session that would start it, so an editor cannot write a
    /// file whose failure waits for somebody to pick that provider.
    fn check_engines(&self, path: &str) -> Result<(), ConfigError> {
        for (profile, p) in &self.providers {
            if let Some(engine) = &p.engine
                && !self.engines.contains_key(engine)
            {
                return Err(ConfigError::NoSuchEngine {
                    path: path.to_string(),
                    profile: profile.clone(),
                    engine: engine.clone(),
                });
            }
        }
        Ok(())
    }

    /// The whole of the load-time rule, and it is about one profile.
    fn check_default(&self, path: &str) -> Result<(), ConfigError> {
        let Some(name) = &self.default else {
            return Ok(());
        };
        let profile = self
            .providers
            .get(name)
            .ok_or_else(|| ConfigError::NoSuchDefault {
                path: path.to_string(),
                name: name.clone(),
            })?;
        let kind = profile.backend.unwrap_or(BackendKind::Ollama);
        // The mock reaches nothing, so it has no destination to declare.
        if kind == BackendKind::Mock {
            return Ok(());
        }
        let url = profile
            .url
            .clone()
            .or_else(|| kind.default_url().map(str::to_string))
            .unwrap_or_default();
        match (is_this_machine(&url), profile.remote) {
            (false, false) => Err(ConfigError::UndeclaredRemoteDefault {
                path: path.to_string(),
                name: name.clone(),
                url,
            }),
            (true, true) => Err(ConfigError::RemoteButLocal {
                path: path.to_string(),
                name: name.clone(),
                url,
            }),
            _ => Ok(()),
        }
    }

    /// The profile a run uses: the one `-p` named, or the file's default, or
    /// none at all.
    fn profile(
        &self,
        named: Option<&str>,
        path: &str,
    ) -> Result<Option<(&str, &Profile)>, ConfigError> {
        let name = match named {
            Some(name) => name,
            None => match &self.default {
                Some(name) => name.as_str(),
                None => return Ok(None),
            },
        };
        match self.providers.get_key_value(name) {
            Some((name, profile)) => Ok(Some((name.as_str(), profile))),
            None => Err(ConfigError::NoSuchProfile {
                path: path.to_string(),
                name: name.to_string(),
            }),
        }
    }
}

impl Config {
    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    pub fn profiles(&self) -> &BTreeMap<String, Profile> {
        &self.providers
    }

    /// The file this would be written to, without asking anything.
    ///
    /// **Never [`crate::config::dir`]**, for the reason that function gives
    /// about `luu stdio`: it prompts when nothing has been chosen, and a prompt
    /// inside a request handler is a hang with a socket attached. A machine that
    /// has not chosen a state directory has nowhere to put this, and says so.
    pub fn path_for_writing() -> Option<PathBuf> {
        let env = crate::config::Env::from_process();
        match crate::config::resolve(&env, |path| path.exists()) {
            crate::config::Resolution::Decided(dir) => Some(dir.join(FILE)),
            _ => None,
        }
    }

    /// TOML, as the file would be written.
    pub fn render(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(&File {
            default: self.default.clone(),
            provider: self.providers.clone(),
            // Carried through the editor untouched. The providers table is the
            // only thing the page writes, and a writer that dropped the
            // postures would delete them the first time somebody saved a URL.
            posture: self.postures.clone(),
            ui: self.ui.clone(),
            resend: self.resend,
            authority: self.authority.clone(),
            engine: self.engines.clone(),
        })
        .map_err(|error| ConfigError::Render {
            message: error.to_string(),
        })
    }

    /// Replaces the file, and **only if what would replace it loads**.
    ///
    /// The check is [`Config::from_toml`] over the rendered text rather than a
    /// second copy of the rules: an editor that could write a file the loader
    /// refuses is an editor that bricks the next run, and the rule that a remote
    /// default must declare itself is the one it would brick it with.
    pub fn write(&self, path: &Path) -> Result<(), ConfigError> {
        let text = self.render()?;
        Self::from_toml(&text, &path.display().to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.display().to_string(),
                source,
            })?;
        }
        // Written beside and renamed over: a crash between the two leaves the
        // old file, and a truncated `config.toml` is a machine that cannot
        // reach a model until somebody edits TOML by hand.
        let temporary = path.with_extension("toml.new");
        std::fs::write(&temporary, &text).map_err(|source| ConfigError::Write {
            path: temporary.display().to_string(),
            source,
        })?;
        std::fs::rename(&temporary, path).map_err(|source| ConfigError::Write {
            path: path.display().to_string(),
            source,
        })
    }

    /// The file as an editor hands it back: a default and a set of profiles.
    ///
    /// The postures are *this* config's, not the editor's: the page does not
    /// write them, and one that arrived without them would be one that erased
    /// them.
    pub fn from_parts(
        &self,
        default: Option<String>,
        providers: BTreeMap<String, Profile>,
    ) -> Self {
        Self {
            default,
            providers,
            postures: self.postures.clone(),
            ui: self.ui.clone(),
            resend: self.resend,
            authority: self.authority.clone(),
            engines: self.engines.clone(),
        }
    }

    /// The file as the icon-theme editor hands it back: `[ui]`, and everything
    /// else this config already had, for [`Config::with_resend`]'s reason. An
    /// empty table is written as no table.
    pub fn with_ui(&self, ui: Ui) -> Self {
        Self {
            ui: (ui != Ui::default()).then_some(ui),
            ..self.clone()
        }
    }

    /// The file as the resend editor hands it back: one table, and everything
    /// else this config already had.
    ///
    /// The mirror of [`Config::from_parts`] and for its reason — the page
    /// writes one table at a time, and a writer that rebuilt the file from what
    /// arrived would delete every table that did not.
    pub fn with_resend(&self, resend: Resend) -> Self {
        Self {
            resend: Some(resend),
            ..self.clone()
        }
    }

    /// The file as the authority editor hands it back, [`Config::with_resend`]'s
    /// reason exactly, one table along.
    pub fn with_authority(&self, authority: AuthorityNotes) -> Self {
        Self {
            authority: Some(authority),
            ..self.clone()
        }
    }

    /// The file as the engines editor hands it back: `[engine.*]`, and
    /// everything else this config already had, [`Config::with_resend`]'s
    /// reason.
    pub fn with_engines(&self, engines: BTreeMap<String, Engine>) -> Self {
        Self {
            engines,
            ..self.clone()
        }
    }

    /// Every engine the file names.
    pub fn engines(&self) -> &BTreeMap<String, Engine> {
        &self.engines
    }

    /// What this machine sets the three rules to, where it says anything.
    pub fn resend(&self) -> Option<Resend> {
        self.resend
    }

    /// What this machine tells a draft or a plan about itself, where it says
    /// anything.
    pub fn authority(&self) -> Option<&AuthorityNotes> {
        self.authority.as_ref()
    }

    /// What the page should look like, as the file asks for it.
    pub fn ui(&self) -> Option<&Ui> {
        self.ui.as_ref()
    }

    /// Every posture the file names.
    pub fn postures(&self) -> &BTreeMap<String, Posture> {
        &self.postures
    }

    /// One posture by name, or the refusal that says where to write it.
    pub fn posture(&self, name: &str, path: &str) -> Result<&Posture, ConfigError> {
        self.postures
            .get(name)
            .ok_or_else(|| ConfigError::NoSuchPosture {
                path: path.to_string(),
                name: name.to_string(),
            })
    }
}

/// What the command line said, before the file is consulted. Every field is
/// optional because "the user did not say" is the question the profile answers,
/// and a clap default would have erased it.
#[derive(Debug, Default, Clone)]
pub struct Flags<'a> {
    pub provider: Option<&'a str>,
    pub backend: Option<BackendKind>,
    pub model: Option<&'a str>,
    pub ollama_url: Option<&'a str>,
    pub openai_url: Option<&'a str>,
    pub api_key_file: Option<&'a Path>,
    /// 0 is the CLI's "unknown window", which is also its "not set".
    pub context_limit: u32,
    /// `--tool-calls`, over the profile's `tool-calls`.
    pub tool_calls: Option<ToolCalls>,
}

/// Where the window a run budgets against came from.
///
/// On the OpenAI API it cannot be *sent*, only budgeted against, so which of
/// the three answered is the difference between a number somebody chose for
/// this run and one a profile has been carrying since the server was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowFrom {
    Flag,
    Profile,
    /// Nothing declared one and the server said which it serves — see
    /// `Backend::window` and
    /// `RECORD/2026-09-30.the-window-the-server-says.completed.md`.
    Server,
    Unset,
}

/// Where the destination itself came from — the question `WindowFrom` asks
/// about the window, and for the same reason: a page is read by somebody who
/// did not type the flags.
///
/// `Default` is the one that matters. It is the mock **nobody asked for**: no
/// `config.toml`, or one with no `default` and no `-p`. A run in that state has
/// nowhere to send, which is a thing to say on the surface a person works in
/// rather than a backend name they have to know to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DestinationFrom {
    Flag,
    Profile,
    Default,
}

/// One destination, resolved. Flags beat the profile field by field; the
/// profile beats the built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub kind: BackendKind,
    /// Empty for the mock, which is nowhere.
    pub url: String,
    pub model: String,
    pub key_file: Option<PathBuf>,
    pub context_limit: u32,
    pub window_from: WindowFrom,
    /// The profile this came from, when one did.
    pub profile: Option<String>,
    /// Whether anything named this destination at all. See [`DestinationFrom`].
    pub destination_from: DestinationFrom,
    /// The engine this profile starts, and its table, when it names one.
    pub engine: Option<(String, Engine)>,
    /// How tool calls travel to and from this destination.
    pub tool_calls: ToolCalls,
    /// Whether anybody *chose* this: `-p`, or any flag at all, or a provider
    /// picked for a session. A destination nobody chose is the file's default
    /// (or the mock with no default), and it follows the file when the file
    /// changes — a new default is used without restarting `serve`.
    pub named: bool,
}

impl Resolved {
    /// The destination a run has when nothing named one: the mock, which is
    /// nowhere. What a test that builds a server without going through the CLI
    /// wants, rather than a struct literal in three files that drift apart.
    pub fn mock() -> Self {
        Self {
            kind: BackendKind::Mock,
            url: String::new(),
            model: "mock".to_string(),
            key_file: None,
            context_limit: 0,
            window_from: WindowFrom::Unset,
            profile: None,
            destination_from: DestinationFrom::Default,
            engine: None,
            tool_calls: ToolCalls::Fenced,
            named: false,
        }
    }

    /// Nothing has said where this run sends: no flag, no profile, no file.
    ///
    /// The whole of the rule, in the crate that owns it. A browser deciding
    /// this for itself would have to know that `mock` means *nowhere* — and
    /// would then call a deliberate `--backend mock` run unconfigured.
    pub fn unconfigured(&self) -> bool {
        self.destination_from == DestinationFrom::Default
    }

    /// The line a run prints before it sends anything.
    ///
    /// `None` for the mock: a line about a destination that does not exist is
    /// noise, and this one has to keep meaning something.
    pub fn line(&self) -> Option<String> {
        if self.kind == BackendKind::Mock {
            return None;
        }
        let destination = format!("{}@{}", self.kind.as_str(), authority_of(&self.url));
        let remote = match is_this_machine(&self.url) {
            true => "",
            false => " (remote)",
        };
        // Only when it is not the default, so every line printed before the
        // key existed reads the same.
        let tools = match self.tool_calls {
            ToolCalls::Native => ", tool calls native",
            ToolCalls::Fenced => "",
        };
        Some(match &self.profile {
            Some(name) => format!("provider: {name} → {destination}{remote}{tools}"),
            None => format!("provider: {destination}{remote}{tools}"),
        })
    }
}

/// The whole resolution: file, then profile, then flags.
pub fn resolve(
    config: &Config,
    path: Option<&Path>,
    flags: Flags<'_>,
) -> Result<Resolved, ConfigError> {
    let shown = path
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    if let (Some(name), None) = (flags.provider, path) {
        return Err(ConfigError::NoConfigFile {
            name: name.to_string(),
            file: FILE.to_string(),
        });
    }
    let profile = config.profile(flags.provider, &shown)?;
    let (name, profile) = match profile {
        Some((name, profile)) => (Some(name.to_string()), profile.clone()),
        None => (None, Profile::default()),
    };

    let kind = flags
        .backend
        .or(profile.backend)
        .unwrap_or(BackendKind::Mock);
    // The two URL flags collapse here: only one of them can be the destination
    // once the backend is known, and carrying both further is how a run ends up
    // reporting the address it did not dial.
    let flag_url = match kind {
        BackendKind::Mock => None,
        BackendKind::Ollama => flags.ollama_url,
        BackendKind::Openai => flags.openai_url,
    };
    let url = flag_url
        .map(str::to_string)
        // A profile's URL is only its own backend's. `--backend openai` over a
        // profile that named an Ollama address would otherwise send an
        // OpenAI-shaped request to `/api/chat`.
        .or_else(|| match profile.backend.unwrap_or(kind) == kind {
            true => profile.url.clone(),
            false => None,
        })
        .or_else(|| kind.default_url().map(str::to_string))
        .unwrap_or_default();

    // A flag beats the file here exactly as it does field by field above, and
    // the mock is a destination when somebody typed it: `--backend mock` is a
    // choice, and the mock a run falls into having read nothing is not.
    let destination_from = match (flags.backend.is_some() || flag_url.is_some(), &name) {
        (true, _) => DestinationFrom::Flag,
        (false, Some(_)) => DestinationFrom::Profile,
        (false, None) => DestinationFrom::Default,
    };

    Ok(Resolved {
        kind,
        url,
        model: flags
            .model
            .map(str::to_string)
            .or(profile.model)
            // Then the model its engine loads: what the session is on is what
            // that server holds, and the built-in default would name another.
            .or_else(|| {
                config
                    .engines
                    .get(profile.engine.as_deref()?)?
                    .model
                    .clone()
            })
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        key_file: flags
            .api_key_file
            .map(Path::to_path_buf)
            .or(profile.key_file.map(|path| expand_home(&path))),
        context_limit: match flags.context_limit {
            0 => profile.context_limit.unwrap_or(0),
            named => named,
        },
        window_from: match (flags.context_limit, profile.context_limit) {
            (0, None) => WindowFrom::Unset,
            (0, Some(_)) => WindowFrom::Profile,
            _ => WindowFrom::Flag,
        },
        // Only while the URL is still the profile's own: a flag that sent
        // this run elsewhere is not a reason to start the profile's server.
        engine: match flag_url.is_none() && flags.backend.is_none_or(|b| Some(b) == profile.backend)
        {
            true => profile
                .engine
                .as_ref()
                .and_then(|engine| Some((engine.clone(), config.engines.get(engine)?.clone()))),
            false => None,
        },
        // The flag over the profile, and fenced where neither says: the
        // transport every earlier recording was made under.
        tool_calls: flags.tool_calls.or(profile.tool_calls).unwrap_or_default(),
        profile: name,
        // A flag beats the file here exactly as it does field by field above,
        // and the mock is a destination when somebody typed it.
        destination_from,
        // Any flag, not only `-p`: a server started with `-m` over the default
        // was told something the file does not say, and re-reading the file
        // would quietly drop it.
        named: flags.provider.is_some()
            || flags.backend.is_some()
            || flags.model.is_some()
            || flags.ollama_url.is_some()
            || flags.openai_url.is_some()
            || flags.api_key_file.is_some()
            || flags.context_limit != 0
            || flags.tool_calls.is_some(),
    })
}

/// `~` against `$HOME`, for a key path written in a file rather than typed.
pub fn expand_home(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => path.to_path_buf(),
    }
}

/// `host:port`, for the line a person reads — and for the host an editor
/// asks somebody to type back.
pub fn authority_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

/// Whether a URL names this machine.
///
/// **A box on the LAN does not.** The commitment says *the machine*, not *the
/// building*: `192.168.1.40` is somewhere the tokens travel to, and nothing
/// about the address says so until somebody writes it down.
///
/// A host this cannot parse is treated as remote. The failure that matters is
/// a destination reached without a declaration, so an address nobody can
/// classify asks for the declaration rather than skipping it.
pub fn is_this_machine(url: &str) -> bool {
    let authority = authority_of(url);
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(host, _)| host),
        None => authority
            .split_once(':')
            .map_or(authority, |(host, _)| host),
    };
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "config.toml";

    fn config(text: &str) -> Result<Config, ConfigError> {
        Config::from_toml(text, PATH)
    }

    /// A profile's `engine` names a table the file has, or the file does not
    /// load; and a profile that names one resolves with it.
    #[test]
    fn a_profile_starts_the_engine_it_names() {
        let error = config("[provider.local]\nbackend = \"openai\"\nengine = \"gone\"\n")
            .expect_err("an engine nobody wrote down");
        assert!(matches!(error, ConfigError::NoSuchEngine { .. }));

        let text = "[engine.gemma]\nkind = \"llama\"\nbinary = \"managed\"\nport = 8081\n\
                    args = [\"-hf\", \"ggml-org/gemma-3-1b-it-GGUF\"]\n\n\
                    [provider.gemma]\nbackend = \"openai\"\nurl = \"http://127.0.0.1:8081/v1\"\n\
                    engine = \"gemma\"\n";
        let resolved = resolved(
            text,
            Flags {
                provider: Some("gemma"),
                ..Default::default()
            },
        );
        let (name, engine) = resolved.engine.expect("it names one");
        assert_eq!(name, "gemma");
        assert_eq!(engine.kind, EngineKind::Llama);
        assert_eq!(engine.port, Some(8081));

        // A flag that sends the run elsewhere is not a reason to start it.
        let elsewhere = resolved_with_flag(text);
        assert!(elsewhere.engine.is_none());

        // And the file round-trips through the writer with the table in it.
        let written = config(text).unwrap().render().unwrap();
        assert!(written.contains("[engine.gemma]"), "{written}");
    }

    fn resolved_with_flag(text: &str) -> Resolved {
        resolved(
            text,
            Flags {
                provider: Some("gemma"),
                openai_url: Some("http://127.0.0.1:9999/v1"),
                ..Default::default()
            },
        )
    }

    fn resolved(text: &str, flags: Flags<'_>) -> Resolved {
        let config = config(text).expect("the file loads");
        resolve(&config, Some(Path::new(PATH)), flags).expect("it resolves")
    }

    #[test]
    fn this_machine_is_loopback_and_a_lan_box_is_not() {
        assert!(is_this_machine("http://127.0.0.1:11434"));
        assert!(is_this_machine("http://localhost:8080/v1"));
        assert!(is_this_machine("http://[::1]:8080/v1"));
        assert!(is_this_machine("http://127.2.3.4:8080"));
        // The commitment says *the machine*, not *the building*.
        assert!(!is_this_machine("http://192.168.1.40:8080/v1"));
        assert!(!is_this_machine("https://openrouter.ai/api/v1"));
        // Unparseable is remote: the failure that matters is a destination
        // reached with no declaration, so it asks for one rather than skipping.
        assert!(!is_this_machine("nonsense"));
    }

    /// The whole of the first-run rule: a mock somebody typed is a
    /// destination, and a mock a run fell into is not.
    #[test]
    fn a_run_that_read_nothing_says_it_has_nowhere_to_send() {
        // No file at all: the shape `serve` has on a machine nobody has
        // configured, and the one the page opens the editor on.
        let empty = Config::default().from_parts(None, BTreeMap::new());
        let fallen = resolve(&empty, None, Flags::default()).expect("it resolves");
        assert_eq!(fallen.destination_from, DestinationFrom::Default);
        assert!(fallen.unconfigured());

        // A file with profiles but no `default`, and no `-p`: still nowhere,
        // and the editor is still the right place to open — with the profiles
        // that exist and no default chosen among them.
        let no_default = resolved("[provider.local]\nbackend = \"ollama\"\n", Flags::default());
        assert!(no_default.unconfigured());

        // Typed. `--backend mock` is a choice, and a run that made it is not
        // asked to configure anything.
        let asked_for = resolved(
            "",
            Flags {
                backend: Some(BackendKind::Mock),
                ..Default::default()
            },
        );
        assert_eq!(asked_for.kind, BackendKind::Mock);
        assert_eq!(asked_for.destination_from, DestinationFrom::Flag);
        assert!(!asked_for.unconfigured());

        // Written down. The file answered, so nothing is missing.
        let from_file = resolved(
            "default = \"local\"\n[provider.local]\nbackend = \"ollama\"\n",
            Flags::default(),
        );
        assert_eq!(from_file.destination_from, DestinationFrom::Profile);
        assert!(!from_file.unconfigured());
    }

    #[test]
    fn a_remote_default_must_say_so() {
        let error = config(
            r#"
            default = "hosted"
            [provider.hosted]
            backend = "openai"
            url = "https://openrouter.ai/api/v1"
            "#,
        )
        .expect_err("every run with no -p would send there");
        assert!(matches!(error, ConfigError::UndeclaredRemoteDefault { .. }));
        assert!(error.to_string().contains("remote = true"));
    }

    #[test]
    fn a_local_default_must_not_claim_to_be_remote() {
        let error = config(
            r#"
            default = "local"
            [provider.local]
            backend = "ollama"
            url = "http://127.0.0.1:11434"
            remote = true
            "#,
        )
        .expect_err("a word written where it is not true is one nobody reads");
        assert!(matches!(error, ConfigError::RemoteButLocal { .. }));
    }

    /// The narrowing the record's later section argued for: `-p hosted` is the
    /// destination being typed, so the declaration is only load-bearing on the
    /// profile nobody has to name.
    #[test]
    fn a_remote_profile_that_is_not_the_default_needs_no_declaration() {
        let text = r#"
            default = "local"
            [provider.local]
            backend = "ollama"
            [provider.hosted]
            backend = "openai"
            url = "https://openrouter.ai/api/v1"
            "#;
        let picked = resolved(
            text,
            Flags {
                provider: Some("hosted"),
                ..Flags::default()
            },
        );
        assert_eq!(picked.kind, BackendKind::Openai);
        assert_eq!(picked.url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn a_default_naming_nothing_is_a_load_error() {
        let error = config("default = \"gone\"\n").expect_err("there is no such profile");
        assert!(matches!(error, ConfigError::NoSuchDefault { .. }));
    }

    #[test]
    fn a_named_profile_that_is_not_there_is_refused_at_use() {
        let config = config("[provider.local]\nbackend = \"ollama\"\n").expect("it loads");
        let error = resolve(
            &config,
            Some(Path::new(PATH)),
            Flags {
                provider: Some("typo"),
                ..Flags::default()
            },
        )
        .expect_err("-p named nothing");
        assert!(matches!(error, ConfigError::NoSuchProfile { .. }));
    }

    #[test]
    fn with_no_file_and_no_flags_it_is_the_mock() {
        let picked = resolve(&Config::default(), None, Flags::default()).expect("it resolves");
        assert_eq!(picked.kind, BackendKind::Mock);
        assert_eq!(picked.model, DEFAULT_MODEL);
        assert_eq!(picked.line(), None, "the mock has nowhere to send");
    }

    #[test]
    fn naming_a_profile_with_no_file_says_which_file_is_missing() {
        let error = resolve(
            &Config::default(),
            None,
            Flags {
                provider: Some("hosted"),
                ..Flags::default()
            },
        )
        .expect_err("there is nowhere for that name to have come from");
        assert!(matches!(error, ConfigError::NoConfigFile { .. }));
    }

    #[test]
    fn the_default_profile_answers_when_no_flag_does() {
        let picked = resolved(
            r#"
            default = "local"
            [provider.local]
            backend = "ollama"
            url = "http://127.0.0.1:11500"
            model = "qwen2.5-coder:14b"
            context-limit = 16384
            "#,
            Flags::default(),
        );
        assert_eq!(picked.kind, BackendKind::Ollama);
        assert_eq!(picked.url, "http://127.0.0.1:11500");
        assert_eq!(picked.model, "qwen2.5-coder:14b");
        assert_eq!(picked.context_limit, 16384);
        assert_eq!(picked.profile.as_deref(), Some("local"));
    }

    #[test]
    fn a_flag_beats_the_profile_field_by_field() {
        let text = r#"
            default = "local"
            [provider.local]
            backend = "ollama"
            url = "http://127.0.0.1:11500"
            model = "qwen2.5-coder:14b"
            context-limit = 16384
            "#;
        let picked = resolved(
            text,
            Flags {
                model: Some("llama3:8b"),
                ..Flags::default()
            },
        );
        // The model moved and nothing else did: `-m` is one field of a
        // destination, not a destination.
        assert_eq!(picked.model, "llama3:8b");
        assert_eq!(picked.url, "http://127.0.0.1:11500");
        assert_eq!(picked.context_limit, 16384);

        let picked = resolved(
            text,
            Flags {
                context_limit: 4096,
                ..Flags::default()
            },
        );
        assert_eq!(picked.context_limit, 4096);

        let picked = resolved(
            text,
            Flags {
                ollama_url: Some("http://127.0.0.1:9999"),
                ..Flags::default()
            },
        );
        assert_eq!(picked.url, "http://127.0.0.1:9999");
    }

    /// A profile's URL belongs to the backend that profile named. Carrying it
    /// across would send an OpenAI-shaped request to Ollama's `/api/chat`.
    #[test]
    fn switching_backend_by_flag_drops_the_profiles_url() {
        let picked = resolved(
            r#"
            default = "local"
            [provider.local]
            backend = "ollama"
            url = "http://127.0.0.1:11500"
            "#,
            Flags {
                backend: Some(BackendKind::Openai),
                ..Flags::default()
            },
        );
        assert_eq!(picked.kind, BackendKind::Openai);
        assert_eq!(picked.url, agent_core::backend::openai::DEFAULT_BASE_URL);
    }

    #[test]
    fn the_line_names_the_profile_and_says_when_it_leaves_the_machine() {
        let text = r#"
            default = "local"
            [provider.local]
            backend = "ollama"
            [provider.hosted]
            backend = "openai"
            url = "https://openrouter.ai/api/v1"
            "#;
        assert_eq!(
            resolved(text, Flags::default()).line().unwrap(),
            "provider: local → ollama@127.0.0.1:11434"
        );
        assert_eq!(
            resolved(
                text,
                Flags {
                    provider: Some("hosted"),
                    ..Flags::default()
                }
            )
            .line()
            .unwrap(),
            "provider: hosted → openai@openrouter.ai (remote)"
        );
        // Typed rather than named: no profile, and the line still says where.
        let picked = resolve(
            &Config::default(),
            None,
            Flags {
                backend: Some(BackendKind::Openai),
                openai_url: Some("https://api.example.com/v1"),
                ..Flags::default()
            },
        )
        .expect("it resolves");
        assert_eq!(
            picked.line().unwrap(),
            "provider: openai@api.example.com (remote)"
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("luu-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the scratch directory");
        dir.join(FILE)
    }

    #[test]
    fn a_write_round_trips_through_the_loader() {
        let path = scratch("round-trip");
        let mut providers = BTreeMap::new();
        providers.insert(
            "local".to_string(),
            Profile {
                backend: Some(BackendKind::Ollama),
                url: Some("http://127.0.0.1:11434".to_string()),
                model: Some("qwen2.5-coder:7b".to_string()),
                ..Profile::default()
            },
        );
        let config = Config::default().from_parts(Some("local".to_string()), providers);
        config.write(&path).expect("it writes");

        let text = std::fs::read_to_string(&path).expect("it is there");
        let read = Config::from_toml(&text, PATH).expect("what was written loads");
        assert_eq!(read.default_name(), Some("local"));
        assert_eq!(
            read.profiles()["local"].url.as_deref(),
            Some("http://127.0.0.1:11434")
        );
        std::fs::remove_file(&path).ok();
    }

    /// The editor is checked by the loader that will read it back, rather than
    /// by a second copy of the rules — so a browser cannot save a file the next
    /// run refuses to start on.
    #[test]
    fn a_write_that_would_not_load_replaces_nothing() {
        let path = scratch("refused");
        std::fs::write(
            &path,
            "default = \"local\"\n\n[provider.local]\nbackend = \"ollama\"\n",
        )
        .expect("the file that is already there");

        let mut providers = BTreeMap::new();
        providers.insert(
            "hosted".to_string(),
            Profile {
                backend: Some(BackendKind::Openai),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                ..Profile::default()
            },
        );
        let error = Config::default()
            .from_parts(Some("hosted".to_string()), providers)
            .write(&path)
            .expect_err("an undeclared remote default is not a file this may write");
        assert!(matches!(error, ConfigError::UndeclaredRemoteDefault { .. }));

        let after = std::fs::read_to_string(&path).expect("still there");
        assert!(after.contains("[provider.local]"), "the old file survived");
        assert!(
            !path.with_extension("toml.new").exists(),
            "nothing is left half-written beside it"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let error = config("[provider.local]\nbackedn = \"ollama\"\n")
            .expect_err("a typo that is silently ignored is a profile that does nothing");
        assert!(matches!(error, ConfigError::Parse { .. }));
    }

    /// `[resend]`, read by the same words the record's header writes.
    ///
    /// The wire word is the point rather than a convenience: a machine's table,
    /// a session's request and the recording of the run all say the arm the
    /// same way, so nobody has to hold a mapping in their head to read one
    /// against another.
    #[test]
    fn the_resend_table_is_read_by_the_headers_own_words() {
        let table =
            config("[resend]\nrepeat = \"once\"\nprune = \"behind\"\nresults = \"cited\"\n")
                .expect("the file loads")
                .resend()
                .expect("the table is there");
        assert_eq!(
            (table.repeat, table.prune, table.results),
            (
                Some(agent_core::context::Repeat::Once),
                Some(agent_core::context::Prune::Behind),
                Some(agent_core::context::Results::Cited)
            ),
        );

        // A table that names one rule states nothing about the other two, and
        // `None` is that and not a default: the same distinction the header
        // draws between a file that says `always` and one written before the
        // field existed.
        let partial = config("[resend]\nrepeat = \"once\"\n")
            .expect("the file loads")
            .resend()
            .expect("the table is there");
        assert_eq!(partial.repeat, Some(agent_core::context::Repeat::Once));
        assert_eq!((partial.prune, partial.results), (None, None));

        // No table at all is a machine that says nothing, which is not the same
        // object as a table that says the defaults.
        assert_eq!(config("").expect("an empty file loads").resend(), None);

        // And a typo is refused rather than ignored, for the reason above it:
        // a rule silently dropped is a run under an arm nobody chose.
        assert!(matches!(
            config("[resend]\nrepaet = \"once\"\n")
                .expect_err("a key nothing reads is a rule that does nothing"),
            ConfigError::Parse { .. }
        ));
        assert!(matches!(
            config("[resend]\nprune = \"aggressively\"\n")
                .expect_err("a value outside the enum is not a rule this machine has"),
            ConfigError::Parse { .. }
        ));
    }

    /// Saving one table does not delete the others.
    ///
    /// The providers writer has this property under a comment saying why, and
    /// this is the second table to need it — the moment there are two editable
    /// tables, a writer that rebuilt the file from what arrived erases whichever
    /// one the page was not editing.
    #[test]
    fn writing_the_resend_table_keeps_every_other_table_in_the_file() {
        let before = config(
            "default = \"local\"\n\
             [provider.local]\nbackend = \"ollama\"\nurl = \"http://127.0.0.1:11434\"\n\
             [posture.container]\npolicy = \"luu.container.toml\"\n\
             [ui]\nicon-theme = \"/tmp/theme.json\"\n",
        )
        .expect("the file loads");

        let rendered = before
            .with_resend(Resend {
                repeat: Some(agent_core::context::Repeat::Once),
                prune: None,
                results: None,
            })
            .render()
            .expect("it renders");

        let after = config(&rendered).expect("what it wrote loads back");
        assert_eq!(after.default_name(), Some("local"));
        assert!(after.profiles().contains_key("local"), "{rendered}");
        assert!(after.postures().contains_key("container"), "{rendered}");
        assert!(after.ui().is_some(), "{rendered}");
        assert_eq!(
            after.resend().expect("the table it wrote").repeat,
            Some(agent_core::context::Repeat::Once),
        );
        // And the keys it was not given stay absent rather than being written
        // as the defaults, which would turn *this machine says nothing* into
        // *this machine chose off*.
        assert!(!rendered.contains("prune"), "{rendered}");
    }
}
