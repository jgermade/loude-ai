// @ts-check
/// Other machines' `luu serve`, reached through this one.
///
/// **The page is always this server's.** Served at `/h/<name>/`, every
/// `./api/…` it asks resolves to `/h/<name>/api/…`, which this server forwards
/// to the host with the host's token — so nothing else in the page knows
/// there is a second server. What does know is here: which host the page is
/// on, the list of them (always this server's, at `/api/hosts`), and asking a
/// host that is not the one on screen, which the project modal does before
/// going there. See
/// `RECORD/2026-10-02.a-project-is-a-host-a-folder-and-a-session.completed.md`.

import { apiHeaders } from "./store.js"

/// The host this page is on: `""` for this machine.
export const currentHost = decodeURIComponent(
  (location.pathname.match(/^\/h\/([^/]+)\//) || [])[1] || "")

/// Where a host's page lives on this server.
export const hostBase = name => (name ? `/h/${encodeURIComponent(name)}/` : "/")

/// One of a host's API paths, from wherever this page is.
export const hostApi = (name, path) => `${hostBase(name)}api/${path}`

async function call(method, path, body) {
  try {
    const res = await fetch(path, {
      method,
      headers: body ? { ...apiHeaders(), "content-type": "application/json" } : apiHeaders(),
      body: body ? JSON.stringify(body) : undefined,
    })
    if (!res.ok) return { ok: false, error: (await res.text()) || `HTTP ${res.status}` }
    return { ok: true, view: await res.json() }
  } catch (e) {
    return { ok: false, error: `${e}` }
  }
}

/// `{ hosts: { name: { url, "token-file" } }, editable, path }`.
export const loadHosts = () => call("GET", "/api/hosts")

/// Replaces every `[host.*]`.
export const saveHosts = hosts => call("PUT", "/api/hosts", { hosts })

/// GET one of a host's API paths.
export const askHost = (name, path) => call("GET", hostApi(name, path))

/// POST to one, with a JSON body.
export const tellHost = (name, path, body) => call("POST", hostApi(name, path), body)

/// The page on another host, carrying `?token=` when this server needs it.
export function goToHost(name) {
  location.href = hostBase(name) + location.search
}
