// @ts-check
/// The page's modals, as the element the platform already has.
///
/// Three of them — settings, the folder picker, the session starter — were a
/// `.modal-backdrop` div with a click handler and no keyboard at all. A
/// `<dialog>` opened with `showModal()` brings four things this page was
/// otherwise going to write by hand: **ESC**, a focus trap, everything behind
/// it made inert, and a `::backdrop` that is one pseudo-element rather than a
/// positioned div holding a `z-index`.
///
/// What `<dialog>` does *not* bring is the page's own state. Whether a modal is
/// on screen is `state.settingsOpen`, `workspace.picker.open` or `ui.starter`,
/// and a dialog the browser closed behind the page's back would leave a
/// component mounted, invisible, and holding the top layer with no way back to
/// it. So every native dismissal is routed to the same close function the close
/// button calls, and the component unmounts the way it always did.
///
/// See `RECORD/2026-09-16.the-modals-are-dialogs.completed.md`.

/// Opens the `<dialog>` with that id as a modal, once the template has put it
/// in the document.
///
/// Called straight from a component's `:setup`, which jq79 runs to completion
/// *before* the template renders — so the element does not exist yet, and this
/// waits for it. The same seam `views/editor/editor.html` uses for Monaco and for
/// `rows.js`, and for the same stated reason: the id is the one thing both
/// sides can name.
///
/// **It waits across frames rather than for one, and that is a fix rather than
/// a precaution.** It used to look once, on the next frame, and give up in
/// silence if the element was not there — which made opening the settings modal
/// a race against its own `:setup`: the call is followed by one `await import`
/// per section, and jq79 renders the template only after all of them resolve.
/// Two sections won the race. Adding a **third** — the one the modal's own
/// comment says its shape makes free — lost it, and the dialog stopped opening
/// at all, with nothing in the console, because a `<dialog>` that was never
/// shown is in the document and invisible. Found by a browser test clicking the
/// new section. See
/// `RECORD/2026-09-18.the-window-rules-are-a-session-fact.completed.md` part 4.
///
/// `locked` is asked at dismissal time rather than read once. The folder picker
/// on a first visit is the case: there is nothing behind it to go back to, so
/// it refuses to close, and a dialog that let ESC through anyway would take the
/// page somewhere it cannot leave.
export function asModal(id, { close, locked = () => false }, frames = 120) {
  requestAnimationFrame(() => {
    const dialog = document.getElementById(id)
    // Not there yet: keep looking until it is, or until enough frames have
    // passed that it is never going to be. The cap is what stops a caller that
    // names an id nothing renders from leaving a callback running for the life
    // of the page — about two seconds at 60Hz, which is far longer than a
    // template takes and short enough to be over before anybody clicks twice.
    if (!dialog) {
      if (frames > 0) asModal(id, { close, locked }, frames - 1)
      return
    }
    if (dialog.open) return

    // ESC — and anything else the browser counts as a dismissal — fires
    // `cancel` first, and `cancel` can be refused. It always is, now: the
    // browser would close the dialog at once, and the page's own close function
    // is what plays the closing animation before it unmounts. See [`closing`].
    dialog.addEventListener("cancel", event => {
      event.preventDefault()
      if (!locked()) close()
    })

    // Whatever closed it, the page's own state is what decides what is on
    // screen, so it is told rather than inferred.
    dialog.addEventListener("close", () => {
      if (locked()) return dialog.showModal()
      close()
    })

    dialog.showModal()
  })
}

/// How long a modal takes to leave; `app.css`'s `modal-out` is the same.
const CLOSE_MS = 140

/// Plays the closing animation of the `<dialog>` with that id, then calls
/// `done` — the page's own close, which unmounts it. Every modal's close
/// function goes through this, so a close button, a click on the backdrop and
/// ESC all leave the same way. Immediate where there is nothing to animate:
/// no such dialog, one the browser already closed, or reduced motion. A second
/// call while it is leaving is ignored rather than closing twice.
export function closing(id, done) {
  const dialog = document.getElementById(id)
  const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches
  if (!dialog || !dialog.open || still) return done()
  if (dialog.classList.contains("closing")) return
  dialog.classList.add("closing")
  setTimeout(done, CLOSE_MS)
}

/// Is any modal on screen? What the page's ESC handler asks before doing
/// anything of its own: a `<dialog>` cancels itself, and a keypress that both
/// closed a modal *and* moved the columns underneath it would be one keypress
/// doing two things.
export function modalOpen() {
  return !!document.querySelector("dialog[open]")
}
