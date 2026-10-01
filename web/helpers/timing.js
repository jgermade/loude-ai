// @ts-check
/// How fast a turn went, out of three instants and the backend's own count.
///
/// `startedAt`, `firstAt` and `endedAt` are the page's clock for a live turn
/// and the stream's `at_ms` for a stored one (`TurnView::first_token_at_ms`).
/// Every number here is a difference, so which clock does not matter.
///
/// **The speed is over the writing, not the wait**: completion tokens divided
/// by first-token-to-end. The wait before the first token is its own number,
/// because it holds everything that is not generation — a model loading, the
/// prompt being processed — and folding it in would make a model look slow
/// for having been switched to. A turn that used tools spent part of that
/// span running them, so its speed is a floor, and it says so with `≥`.
/// `null` wherever the backend did not report a count: zero would be a claim.

/// `{ speed, first, total, tools }`, each `null` when it cannot be known.
export function timing(message, tools = []) {
  const started = message?.startedAt ?? null
  const first = message?.firstAt ?? null
  const ended = message?.endedAt ?? null
  const completion = message?.usage?.completion_tokens ?? null
  const writing = first != null && ended != null ? (ended - first) / 1000 : null
  return {
    speed: completion != null && writing != null && writing > 0.05 ? completion / writing : null,
    first: started != null && first != null ? (first - started) / 1000 : null,
    total: started != null && ended != null ? (ended - started) / 1000 : null,
    tools: (tools || []).length > 0,
  }
}

/// `12.3 s`, `850 ms`: seconds when it is at least one, milliseconds below.
export function seconds(value) {
  if (value == null) return ""
  return value >= 1 ? `${value.toFixed(1)} s` : `${Math.round(value * 1000)} ms`
}

/// `42 tok/s`, `≥ 42 tok/s` for a turn that used tools, `""` when unknown.
export function speedLabel(t) {
  if (t.speed == null) return ""
  return `${t.tools ? "≥ " : ""}${t.speed >= 10 ? Math.round(t.speed) : t.speed.toFixed(1)} tok/s`
}
