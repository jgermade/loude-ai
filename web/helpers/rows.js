// @ts-check
// Rows that stay the same objects while nothing in them changed.
//
// jq79 keeps an `:each` row only when the item at its key is **the same
// object** as last time (`Object.is`); a new object under the same key is
// disposed and drawn again from nothing. And it never wakes a row for a
// property assigned inside an item (`store.js`, `replaceLast`), so a row that
// changed has to be a new object, and is redrawn.
//
// The transcript is recomputed on every token, from fresh objects, so every
// row of it — every earlier message, every snippet, every `<code>` and its
// `innerHTML` — was torn down and rebuilt on every token of a reply, not only
// the one being written. A keeper hands back last time's object for a key
// whose fields are all the same as last time, and a new one only where
// something changed: the rows a token did not touch are left standing.

/// Equal enough to keep the old value: the same thing, or an array or a plain
/// object whose members are each the same thing. One level, which is what a
/// part's fields and a row's `parts` and `timing` need.
function same(a, b) {
  if (Object.is(a, b)) return true
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((item, i) => Object.is(item, b[i]))
  }
  const plain = value => value !== null && typeof value === "object" &&
    Object.getPrototypeOf(value) === Object.prototype
  if (plain(a) && plain(b)) {
    const keys = Object.keys(a)
    return keys.length === Object.keys(b).length && keys.every(key => Object.is(a[key], b[key]))
  }
  return false
}

export function keeper() {
  let rows = new Map()
  let next = new Map()

  /// Last time's object for `key` if every field is the same, else `fields`.
  const keep = (key, fields) => {
    const had = rows.get(key)
    const kept = had &&
      Object.keys(fields).length === Object.keys(had).length &&
      Object.keys(fields).every(name => same(had[name], fields[name]))
      ? had
      : fields
    next.set(key, kept)
    return kept
  }

  /// Ends a pass: what was kept in it is what the next pass compares against,
  /// and a key not seen in it (a session switched, a fold closed) is gone.
  const sweep = () => {
    rows = next
    next = new Map()
  }

  return { keep, sweep }
}
