//! Public runner proof for the compile-fail extension.
//!
//! The real proof of this crate is its `compile_fail` doctest suite,
//! which the README runs with `cargo test --doc`; an integration test
//! cannot re-run doctests, so this target proves the documented probe
//! count only. Characterization pins the count exactly against the
//! literal: relaxing any of the four private boundaries fails loudly in
//! the doctest suite, and a drifted count fails here.

use tina_extension_compile_fail::documented_compile_fails;

/// Documented public runner path: the count guard behind `cargo test`.
#[test]
fn public_smoke() {
    assert_eq!(documented_compile_fails(), 4);
}

/// Pins the exact number of compile-fail probes the README advertises:
/// no backdoor module, no forged permits, no forged bridge pressure,
/// no forged capability rows.
#[test]
fn public_characterization() {
    assert_eq!(documented_compile_fails(), 4);
}
