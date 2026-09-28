// @ts-check
/// Settings → Engines, as calls: the model servers luu starts, and the
/// downloads of luu's own copies. Every answer is the whole view again, so the
/// page never works out a server's state for itself. See `crate::engines` and
/// `RECORD/2026-09-28.a-model-server-luu-starts.completed.md`.
import { apiHeaders } from "./store.js"

async function call(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: { ...apiHeaders(), ...(body === undefined ? {} : { "content-type": "application/json" }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  })
  if (!res.ok) return { ok: false, error: (await res.text()) || `${res.status}` }
  return { ok: true, view: await res.json() }
}

export const loadEngines = () => call("GET", "./api/engines")
/// The whole `[engine.*]` table: the page writes one table at a time, the
/// way it writes the providers.
export const saveEngines = engines => call("PUT", "./api/engines", { engines })
// An empty JSON body, and it is load-bearing: see `start_engine`.
export const startEngine = name => call("POST", `./api/engines/${encodeURIComponent(name)}/start`, {})
export const stopEngine = name => call("POST", `./api/engines/${encodeURIComponent(name)}/stop`, {})
export const installEngine = (kind, version, variant) =>
  call("POST", "./api/engines/install", { kind, version: version || null, variant: variant || null })

/// What a profile for this engine is: the backend each kind speaks, and the
/// URL its port answers on. The same two facts `EngineKind::url` states.
export function profileFor(name, kind, port) {
  return kind === "ollama"
    ? { backend: "ollama", url: `http://127.0.0.1:${port}`, engine: name }
    : { backend: "openai", url: `http://127.0.0.1:${port}/v1`, engine: name }
}

/// Each kind's own port — `EngineKind::default_port`.
export const defaultPort = kind => ({ ollama: 11434, mlx: 11234 })[kind] || 8080

/// Every model on this machine — ollama, llama.cpp, Hugging Face, luu's own —
/// each with a `reference` for `config.toml` and a `label` to show.
export const loadCatalog = () => call("GET", "./api/models")

const SOURCES = ["ollama", "llama.cpp", "huggingface", "luu"]

/// `name (source)` for a reference into the catalog, the text itself for any
/// other model name — the same rule as `models::label` on the server.
export function modelLabel(text) {
  if (!text) return ""
  const at = text.indexOf(":")
  const source = at > 0 ? text.slice(0, at) : ""
  if (!SOURCES.includes(source)) return text
  const rest = text.slice(at + 1)
  const name = source === "ollama" ? rest : rest.split("/").pop().replace(/\.gguf$/, "")
  return `${name} (${source})`
}
