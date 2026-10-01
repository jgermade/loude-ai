//! The page's half of the version pair.
//!
//! `store.js` sends `{protocol, format}` on connect and the host refuses
//! anything else out loud — which is worth exactly as much as the two numbers
//! being right. They drifted the day `record::FORMAT` went to 8: the page kept
//! saying 7, the server refused every `hello` and closed, and the UI could not
//! open a session at all until a browser test went looking. Two constants in
//! two languages want a test that reads both, and this is it.
//!
//! See `RECORD/2026-09-08.a-test-that-clicks-approve.completed.md`.

const STORE: &str = include_str!("../../../web/store.js");

/// `const NAME = <digits>` at the top of the file, as a number.
fn declared(name: &str) -> u32 {
    let needle = format!("const {name} = ");
    let line = STORE
        .lines()
        .find(|line| line.starts_with(&needle))
        .unwrap_or_else(|| panic!("store.js declares no `{needle}…`"));
    line[needle.len()..]
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("`{line}` is not a number the host could be compared against"))
}

#[test]
fn the_page_speaks_the_protocol_this_binary_serves() {
    assert_eq!(
        declared("PROTOCOL"),
        agent_core::protocol::VERSION,
        "web/store.js and agent_core::protocol::VERSION disagree: \
         the host refuses the page's hello and closes it"
    );
}

#[test]
fn the_page_reads_the_record_format_this_binary_writes() {
    assert_eq!(
        declared("FORMAT"),
        agent_core::record::FORMAT,
        "web/store.js and agent_core::record::FORMAT disagree: \
         the host refuses the page's hello and closes it"
    );
}
