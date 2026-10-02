// @ts-check
/// Where a session's commands may run, as a list a picker offers: the
/// server's own policy file and every `[posture.<name>]` in `config.toml`,
/// each on its own runtime first and — where it is contained — on every other
/// container runtime after it. One list for the terminal's foot, which moves
/// a session, and the session starter, which opens one. See
/// `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
///
/// The value is `posture|runtime`: the posture's name (`""` is the server's
/// own) and the runtime where it is not the one that posture names.

/// How a place reads: by where it runs, which is the question both pickers
/// answer.
const runs = place => !place ? "?"
  : place.runtime === "direct" ? "direct (no container)"
  : place.image ? `${place.runtime} · ${place.image}` : place.runtime

/// `[{ value, posture, runtime, name, label, title, disabled }]` out of what
/// `/api/postures` answered. A runtime this machine does not have is in the
/// list, disabled, saying so.
export function placeOptions(postures) {
  const option = (name, label, place, declared) => {
    const runtime = place?.runtime === declared ? "" : place?.runtime || ""
    return {
      value: `${name}|${runtime}`,
      posture: name,
      runtime,
      name: label,
      label: runs(place),
      title: place?.missing
        ? `${label} — ${place.missing}`
        : `${label} · ${place?.policy || ""}${place?.gap ? " · the container keeps its network" : ""}`,
      disabled: !!place?.missing,
    }
  }
  const variants = (name, label, places) =>
    (places || []).map(place => option(name, label, place, places[0]?.runtime))
  return [
    ...variants("", "server default", postures?.own),
    ...Object.keys(postures?.postures || {}).flatMap(name =>
      variants(name, name, postures?.places?.[name])),
  ]
}

/// `value` if it is still a place that can be chosen, else the server's own.
/// A remembered place may name a posture since taken out of `config.toml`, or
/// a runtime this machine no longer has.
export function stillThere(postures, value) {
  const found = placeOptions(postures).find(option => option.value === value && !option.disabled)
  return found ? found.value : "|"
}
