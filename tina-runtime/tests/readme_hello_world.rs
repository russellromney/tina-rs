//! Byte-sync proof: the `hello_world.rs` program quoted in
//! `docs/tina-user-guide/02-first-isolate.md` must match the checked-in
//! source verbatim. The quoted program is the canonical first isolate
//! form; editing one without the other fails this test.

const FENCE: &str = "```rust\n//! Smallest runnable Tina program.";

fn quoted_program() -> String {
    let page = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/tina-user-guide/02-first-isolate.md"
    );
    let text = std::fs::read_to_string(page).expect("read guide page");
    let start = text
        .find(FENCE)
        .expect("guide page quotes the hello_world program");
    let body_start = start + "```rust\n".len();
    let end = text[body_start..]
        .find("\n```")
        .map(|i| body_start + i)
        .expect("fenced program closes");
    text[body_start..end].to_string()
}

#[test]
fn guide_quote_is_byte_identical_to_hello_world() {
    let source = include_str!("../examples/hello_world.rs");
    let quoted = quoted_program();
    assert_eq!(
        quoted.trim_end_matches('\n'),
        source.trim_end_matches('\n'),
        "docs/tina-user-guide/02-first-isolate.md's quoted program drifted from \
         tina-runtime/examples/hello_world.rs; the fenced block must match the \
         source verbatim."
    );
}
