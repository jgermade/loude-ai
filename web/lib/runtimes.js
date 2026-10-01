// @ts-check
/// Settings → Runtimes, as calls: the container runtimes probed, the images
/// they have, a build of one that is missing, the postures, and the host
/// shell's rule. Every write answers with the whole view again. See
/// `crate::runtimes` and `RECORD/2026-10-01.runtimes-in-settings.completed.md`.
import { apiHeaders, loadPostures } from "./store.js"

async function call(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: { ...apiHeaders(), ...(body === undefined ? {} : { "content-type": "application/json" }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  })
  if (!res.ok) return { ok: false, error: (await res.text()) || `${res.status}` }
  return { ok: true, view: await res.json() }
}

/// Probes every container runtime, so it starts a process per installed one:
/// asked when the section opens and on *check again*, never on a timer.
export const loadRuntimes = () => call("GET", "./api/runtimes")
/// The tasks alone — installs, starts, builds — which start nothing: what is
/// polled while one runs.
export const loadTasks = () => call("GET", "./api/runtimes/tasks")
export const buildImage = (runtime, image) =>
  call("POST", `./api/runtimes/${encodeURIComponent(runtime)}/build`, { image })
// An empty JSON body, and it is load-bearing: see `start_engine`.
export const installRuntime = runtime =>
  call("POST", `./api/runtimes/${encodeURIComponent(runtime)}/install`, {})
export const startRuntime = runtime =>
  call("POST", `./api/runtimes/${encodeURIComponent(runtime)}/start`, {})
export const saveHostShell = host => call("PUT", "./api/terminal", { host })

/// The whole `[posture.*]` table, the way the page writes every table. The
/// terminal's picker reads the postures too, so it is told.
export async function savePostures(postures) {
  const result = await call("PUT", "./api/postures", { postures })
  if (result.ok) await loadPostures()
  return result
}
