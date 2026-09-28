//! The models already on this machine, wherever a tool put them: ollama's
//! store, llama.cpp's cache, Hugging Face's hub cache, and luu's own
//! `<state dir>/models`. Nothing is copied or moved — each is listed where it
//! is, named by the tool that owns it, and read by path.
//!
//! **A model is named by a reference, not by its path**: `ollama:qwen2.5-coder:7b`,
//! `huggingface:ggml-org/gemma-3-270m-it-GGUF/gemma-3-270m-it-Q8_0.gguf`. A path
//! into ollama's store is a content hash that changes when the model is
//! pulled again, and a file that says `sha256-60e05f…` is one nobody can read.
//! The reference is what `config.toml` carries; [`resolve`] finds the file.
//! See `RECORD/2026-09-28.a-model-server-luu-starts.completed.md`.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "llama.cpp")]
    LlamaCpp,
    #[serde(rename = "huggingface")]
    HuggingFace,
    #[serde(rename = "luu")]
    Luu,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Ollama => "ollama",
            Source::LlamaCpp => "llama.cpp",
            Source::HuggingFace => "huggingface",
            Source::Luu => "luu",
        }
    }

    const ALL: [Source; 4] = [
        Source::Ollama,
        Source::LlamaCpp,
        Source::HuggingFace,
        Source::Luu,
    ];

    fn parse(prefix: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == prefix)
    }
}

/// One model on disk.
#[derive(Debug, Clone, Serialize)]
pub struct LocalModel {
    /// What `config.toml` writes: `<source>:<name>`.
    pub reference: String,
    /// The owning tool's own name for it.
    pub name: String,
    pub source: Source,
    /// `name (source)`, and the format where it is not GGUF: what a person
    /// picks from.
    pub label: String,
    /// `gguf`; `mlx` for MLX weights (a Hugging Face directory whose
    /// `config.json` carries MLX's `quantization`, or ollama's MLX models);
    /// `safetensors` for an unquantized Hugging Face directory. Everything is
    /// listed, whether or not an engine here can run it — see
    /// [`LocalModel::runs_on`].
    pub format: &'static str,
    pub size: u64,
    pub path: String,
}

impl LocalModel {
    /// Whether llama.cpp can run it.
    pub fn gguf(&self) -> bool {
        self.format == "gguf"
    }

    /// Whether an engine of this kind can load it, and why not when it
    /// cannot — the sentence the page shows beside a model it greys out.
    ///
    /// **ollama's MLX models run on ollama only.** Each tensor is its own
    /// safetensors blob, which a directory of links can present as a model
    /// directory; that was tried on `qwen3.8:27b-mlx`. `mlx_lm` 0.31.3 refused
    /// the weights, and mlx-serve v26.9.6 loaded all of them and failed at the
    /// first matmul — ollama quantizes them as NVFP4 in compressed-tensors
    /// (`weight.scale`, `weight.global_scale`), which neither reads. See the
    /// engines record.
    pub fn runs_on(&self, kind: crate::provider::EngineKind) -> Result<(), String> {
        use crate::provider::EngineKind;
        match (kind, self.format, self.source) {
            (EngineKind::Llama, "gguf", _) => Ok(()),
            (EngineKind::Llama, _, Source::Ollama) => Err(
                "ollama's own MLX format: llama.cpp loads GGUF only, and only ollama runs this"
                    .into(),
            ),
            (EngineKind::Llama, format, _) => Err(format!(
                "{format} weights: llama.cpp loads GGUF only — an mlx engine (mlx-serve) runs this"
            )),
            // mlx-serve embeds llama.cpp, so a GGUF is as good as MLX to it.
            (EngineKind::Mlx, "gguf", _) => Ok(()),
            (EngineKind::Mlx, _, Source::Ollama) => Err(
                "ollama's own MLX format (NVFP4 in compressed-tensors), which mlx-serve cannot \
                 load: only ollama runs this"
                    .into(),
            ),
            (EngineKind::Mlx, _, _) => Ok(()),
            (EngineKind::Ollama, _, Source::Ollama) => Ok(()),
            (EngineKind::Ollama, _, _) => {
                Err("not in ollama's store: ollama serves the models it pulled".into())
            }
        }
    }
}

/// Where each tool keeps its models, each overridable the way the tool itself
/// allows: `OLLAMA_MODELS`, `LLAMA_CACHE`, `HF_HUB_CACHE`/`HF_HOME`.
pub fn root(source: Source) -> Option<PathBuf> {
    let env = |key: &str| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let home = env("HOME");
    match source {
        Source::Ollama => env("OLLAMA_MODELS").or_else(|| Some(home?.join(".ollama/models"))),
        Source::LlamaCpp => env("LLAMA_CACHE").or_else(|| {
            if cfg!(target_os = "macos") {
                Some(home?.join("Library/Caches/llama.cpp"))
            } else {
                env("XDG_CACHE_HOME")
                    .or_else(|| Some(home?.join(".cache")))
                    .map(|cache| cache.join("llama.cpp"))
            }
        }),
        Source::HuggingFace => env("HF_HUB_CACHE")
            .or_else(|| env("HF_HOME").map(|h| h.join("hub")))
            .or_else(|| Some(home?.join(".cache/huggingface/hub"))),
        Source::Luu => Some(
            crate::provider::Config::path_for_writing()?
                .parent()?
                .join("models"),
        ),
    }
}

/// Every model on the machine, by source and then by name.
pub fn catalog() -> Vec<LocalModel> {
    let mut all = Vec::new();
    for source in Source::ALL {
        let Some(root) = root(source) else { continue };
        if !root.is_dir() {
            continue;
        }
        match source {
            Source::Ollama => ollama(&root, &mut all),
            Source::HuggingFace => hugging_face(&root, &mut all),
            Source::LlamaCpp | Source::Luu => files(source, &root, &mut all),
        }
    }
    all.sort_by(|a, b| (a.source, &a.name).cmp(&(b.source, &b.name)));
    all
}

/// Whether a string is a reference [`resolve`] reads, rather than a model
/// name a server knows.
pub fn is_reference(text: &str) -> bool {
    text.split_once(':')
        .is_some_and(|(prefix, rest)| Source::parse(prefix).is_some() && !rest.is_empty())
}

/// The file a reference names, if it is on this machine.
pub fn resolve(reference: &str) -> Option<LocalModel> {
    if !is_reference(reference) {
        return None;
    }
    catalog().into_iter().find(|m| m.reference == reference)
}

/// `name (source)` for a reference, or the text itself when it is not one.
pub fn label(text: &str) -> String {
    match text.split_once(':') {
        Some((prefix, rest)) if Source::parse(prefix).is_some() => {
            let name = match Source::parse(prefix) {
                Some(Source::HuggingFace) | Some(Source::LlamaCpp) | Some(Source::Luu) => {
                    stem(rest)
                }
                _ => rest.to_string(),
            };
            format!("{name} ({prefix})")
        }
        _ => text.to_string(),
    }
}

fn stem(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".gguf").unwrap_or(file).to_string()
}

fn entry(
    source: Source,
    name: String,
    key: String,
    path: &Path,
    format: &'static str,
) -> LocalModel {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let shown = match source {
        Source::Ollama => name.clone(),
        _ => stem(&name),
    };
    let label = match format {
        "gguf" => format!("{shown} ({})", source.as_str()),
        other => format!("{shown} ({}, {other})", source.as_str()),
    };
    LocalModel {
        reference: format!("{}:{key}", source.as_str()),
        name: shown,
        source,
        label,
        format,
        size,
        path: path.display().to_string(),
    }
}

fn is_gguf(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"GGUF"
}

/// A file worth listing: a GGUF that is a model, and the first part of one
/// split in several. A vision projector (`mmproj-…`) is half of a model and
/// not something a server is started on.
fn listable(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    lower.ends_with(".gguf")
        && !lower.starts_with("mmproj")
        && (!lower.contains("-of-") || lower.contains("-00001-of-"))
}

/// ollama's store: `manifests/<registry>/<namespace>/<model>/<tag>`, each a
/// JSON whose `application/vnd.ollama.image.model` layer is the GGUF under
/// `blobs/`. MLX models have no such layer, and are listed as `mlx`.
fn ollama(root: &Path, out: &mut Vec<LocalModel>) {
    #[derive(serde::Deserialize)]
    struct Manifest {
        layers: Vec<Layer>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Layer {
        media_type: String,
        digest: String,
        #[serde(default)]
        size: u64,
    }
    let manifests = root.join("manifests");
    for path in walk(&manifests, 4) {
        let Ok(relative) = path.strip_prefix(&manifests) else {
            continue;
        };
        let parts: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let [registry, rest @ .., model, tag] = parts.as_slice() else {
            continue;
        };
        let name = match (registry.as_str(), rest) {
            ("registry.ollama.ai", [ns]) if ns == "library" => format!("{model}:{tag}"),
            ("registry.ollama.ai", ns) => format!("{}/{model}:{tag}", ns.join("/")),
            (registry, []) => format!("{registry}/{model}:{tag}"),
            (registry, ns) => format!("{registry}/{}/{model}:{tag}", ns.join("/")),
        };
        let Some(manifest) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Manifest>(&text).ok())
        else {
            continue;
        };
        let model_layer = manifest
            .layers
            .iter()
            .find(|l| l.media_type == "application/vnd.ollama.image.model");
        match model_layer {
            Some(layer) => {
                let blob = root.join("blobs").join(layer.digest.replace(':', "-"));
                let format = if is_gguf(&blob) { "gguf" } else { "other" };
                out.push(entry(Source::Ollama, name.clone(), name, &blob, format));
            }
            None if manifest
                .layers
                .iter()
                .any(|l| l.media_type.ends_with(".tensor")) =>
            {
                // Many blobs rather than one file, so its size is the layers'.
                let mut model = entry(Source::Ollama, name.clone(), name, &path, "mlx");
                model.size = manifest.layers.iter().map(|l| l.size).sum();
                out.push(model);
            }
            None => {}
        }
    }
}

/// The hub cache: `models--<org>--<repo>/snapshots/<revision>/<file>`, each a
/// symlink into `blobs/`. Named `<org>/<repo>/<file>` — one revision per file,
/// the newest by modification time, since a reference that pinned a revision
/// would stop resolving the day the repository moved.
fn hugging_face(root: &Path, out: &mut Vec<LocalModel>) {
    let Ok(repos) = std::fs::read_dir(root) else {
        return;
    };
    for repo in repos.flatten() {
        let dir = repo.file_name().to_string_lossy().into_owned();
        let Some(id) = dir.strip_prefix("models--") else {
            continue;
        };
        let id = id.replacen("--", "/", 1);
        let snapshots = repo.path().join("snapshots");
        let mut seen: std::collections::BTreeMap<String, (std::time::SystemTime, PathBuf)> =
            Default::default();
        for path in walk(&snapshots, 4) {
            let Ok(relative) = path.strip_prefix(&snapshots) else {
                continue;
            };
            // Past the revision directory.
            let file: PathBuf = relative.components().skip(1).collect();
            let file = file.to_string_lossy().into_owned();
            if !listable(&file) || !path.exists() {
                continue;
            }
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if seen.get(&file).is_none_or(|(when, _)| modified > *when) {
                seen.insert(file, (modified, path));
            }
        }
        for (file, (_, path)) in seen {
            let key = format!("{id}/{file}");
            out.push(entry(Source::HuggingFace, key.clone(), key, &path, "gguf"));
        }
        // And the repository itself, when it is a model directory rather than
        // a shelf of GGUF files: what `mlx_lm` loads. The newest revision.
        let newest = std::fs::read_dir(&snapshots)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .max_by_key(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH)
            });
        if let Some(dir) = newest
            && let Some(model) = weights_dir(Source::HuggingFace, id.clone(), &dir)
        {
            out.push(model);
        }
    }
}

/// A Hugging Face–shaped model directory — `config.json` beside
/// `*.safetensors` — as one entry, `mlx` where the config carries MLX's own
/// `quantization` table and `safetensors` otherwise. Sized by its weights.
fn weights_dir(source: Source, key: String, dir: &Path) -> Option<LocalModel> {
    let config = std::fs::read_to_string(dir.join("config.json")).ok()?;
    let weights: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "safetensors") && p.is_file())
        .collect();
    if weights.is_empty() {
        return None;
    }
    let quantized = serde_json::from_str::<serde_json::Value>(&config)
        .ok()
        .is_some_and(|c| c.get("quantization").is_some());
    let format = if quantized { "mlx" } else { "safetensors" };
    let mut model = entry(source, key.clone(), key, dir, format);
    model.size = weights
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    Some(model)
}

/// A directory of GGUF files, named by their path inside it: llama.cpp's
/// cache, and luu's own.
fn files(source: Source, root: &Path, out: &mut Vec<LocalModel>) {
    for path in walk(root, 6) {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let key = relative.to_string_lossy().replace('\\', "/");
        let file = key.rsplit('/').next().unwrap_or(&key).to_string();
        if file == "config.json" {
            // A model directory, named by the directory.
            let dir = path.parent().unwrap_or(root);
            let key = dir
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if !key.is_empty()
                && let Some(model) = weights_dir(source, key, dir)
            {
                out.push(model);
            }
            continue;
        }
        if !listable(&file) {
            continue;
        }
        out.push(entry(source, key.clone(), key, &path, "gguf"));
    }
}

/// Every file under `dir`, following symlinks to files (the hub cache is made
/// of them) but never into directories, to a bounded depth.
fn walk(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth > 0 {
                found.extend(walk(&path, depth - 1));
            }
        } else if kind.is_file() || (kind.is_symlink() && path.is_file()) {
            found.push(path);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("luu-models-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_ollama_store_is_read_by_its_manifests() {
        let root = scratch("ollama");
        let manifest = root.join("manifests/registry.ollama.ai/library/qwen2.5-coder/7b");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:abc"}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        std::fs::write(root.join("blobs/sha256-abc"), b"GGUF....").unwrap();
        let mlx = root.join("manifests/registry.ollama.ai/library/qwen3.8/27b-mlx");
        std::fs::create_dir_all(mlx.parent().unwrap()).unwrap();
        std::fs::write(
            &mlx,
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.tensor","digest":"sha256:def"}]}"#,
        )
        .unwrap();

        let mut found = Vec::new();
        ollama(&root, &mut found);
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(found[0].reference, "ollama:qwen2.5-coder:7b");
        assert_eq!(found[0].label, "qwen2.5-coder:7b (ollama)");
        assert!(found[0].gguf());
        assert_eq!(
            found[0].path,
            root.join("blobs/sha256-abc").display().to_string()
        );
        assert_eq!(found[1].label, "qwen3.8:27b-mlx (ollama, mlx)");
        assert!(!found[1].gguf());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_hub_cache_is_read_by_repository_and_file() {
        let root = scratch("hub");
        let snap = root.join("models--ggml-org--gemma-3-270m-it-GGUF/snapshots/e764");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("gemma-3-270m-it-Q8_0.gguf"), b"GGUF").unwrap();
        std::fs::write(snap.join("mmproj-gemma.gguf"), b"GGUF").unwrap();
        std::fs::write(snap.join("big-00002-of-00003.gguf"), b"GGUF").unwrap();
        let mut found = Vec::new();
        hugging_face(&root, &mut found);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].reference,
            "huggingface:ggml-org/gemma-3-270m-it-GGUF/gemma-3-270m-it-Q8_0.gguf"
        );
        assert_eq!(found[0].label, "gemma-3-270m-it-Q8_0 (huggingface)");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Which engine loads what — the rule the page greys models out by.
    #[test]
    fn each_engine_loads_what_it_can_and_says_why_not() {
        use crate::provider::EngineKind::{Llama, Mlx, Ollama};
        let model = |source, format| LocalModel {
            reference: String::new(),
            name: String::new(),
            source,
            label: String::new(),
            format,
            size: 0,
            path: String::new(),
        };
        let ollama_gguf = model(Source::Ollama, "gguf");
        let ollama_mlx = model(Source::Ollama, "mlx");
        let hub_mlx = model(Source::HuggingFace, "mlx");
        let hub_gguf = model(Source::HuggingFace, "gguf");

        assert!(ollama_gguf.runs_on(Llama).is_ok());
        assert!(hub_gguf.runs_on(Llama).is_ok());
        assert!(hub_mlx.runs_on(Llama).unwrap_err().contains("GGUF only"));
        assert!(
            ollama_mlx
                .runs_on(Llama)
                .unwrap_err()
                .contains("only ollama")
        );

        assert!(hub_mlx.runs_on(Mlx).is_ok());
        assert!(
            ollama_gguf.runs_on(Mlx).is_ok(),
            "mlx-serve embeds llama.cpp"
        );
        assert!(ollama_mlx.runs_on(Mlx).unwrap_err().contains("NVFP4"));

        assert!(ollama_mlx.runs_on(Ollama).is_ok());
        assert!(hub_mlx.runs_on(Ollama).is_err());
    }

    #[test]
    fn a_reference_is_told_apart_from_a_model_name() {
        assert!(is_reference("ollama:qwen2.5-coder:7b"));
        assert!(is_reference("luu:gemma/gemma-3-1b-it-Q4_K_M.gguf"));
        assert!(!is_reference("qwen2.5-coder:7b"));
        assert!(!is_reference("ollama:"));
        assert_eq!(
            label("ollama:qwen2.5-coder:7b"),
            "qwen2.5-coder:7b (ollama)"
        );
        assert_eq!(label("luu:gemma/g-Q4.gguf"), "g-Q4 (luu)");
        assert_eq!(label("gpt-4o"), "gpt-4o");
    }
}
