//! README byte-sync proof.
//!
//! The `EchoConnection` snippet in the top-level `README.md` must be
//! byte-identical to the checked-in source between the
//! `readme:echo-isolate` markers in `src/lib.rs`. Edit one without the
//! other and this test fails, so the documented isolate cannot rot away
//! from the isolate that actually compiles and runs.

const BEGIN: &str = "// readme:echo-isolate:begin";
const END: &str = "// readme:echo-isolate:end";

/// Returns the source lines strictly between the two markers.
fn echo_isolate_region() -> String {
    let lib = include_str!("../src/lib.rs");
    let after_begin = lib
        .split_once(BEGIN)
        .expect("lib.rs has the begin marker")
        .1;
    let region = after_begin
        .split_once(END)
        .expect("lib.rs has the end marker")
        .0;
    // Drop the newline that follows BEGIN and the indentation before END.
    region.trim_matches('\n').to_string()
}

fn readme() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md");
    std::fs::read_to_string(path).expect("read top-level README.md")
}

#[test]
fn readme_echo_snippet_is_byte_identical_to_source() {
    let region = echo_isolate_region();
    assert!(
        region.contains("pub enum EchoConnectionMsg"),
        "marker region should hold the connection isolate, got:\n{region}",
    );
    let readme = readme();
    assert!(
        readme.contains(&region),
        "README.md echo snippet drifted from src/lib.rs; the fenced block must \
         match the source between the readme:echo-isolate markers verbatim.\n\n\
         Expected to find this exact text in README.md:\n\n{region}",
    );
}
