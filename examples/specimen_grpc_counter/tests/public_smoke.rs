//! Public runner proof for the native gRPC counter specimen.
//!
//! Characterization pins the typed outcomes of the copied native-client
//! path over real in-process h2c: `Increment(delta=7)` on a fresh
//! counter returns 7, `Forbidden` returns `PermissionDenied`, and the
//! connection survives a client cancel. Public smoke exercises the
//! documented `run_smoke()` binary path.

use specimen_grpc_counter::{run_smoke, start_server};
use tina_http::GrpcStatusCode;

/// Documented public runner path: `run_smoke()`
/// (`cargo run --manifest-path examples/specimen_grpc_counter/Cargo.toml`).
#[test]
fn public_smoke() {
    let value = run_smoke().expect("specimen smoke");
    assert_eq!(value, 7);
}

/// Pins the exact unary outcomes through the native client.
#[test]
fn public_characterization() {
    let server = start_server().expect("start specimen server");
    let smoke = server.native_grpc_smoke().expect("native gRPC smoke");
    // First call on a fresh counter: 0 + delta(7).
    assert_eq!(smoke.increment_value, 7);
    // Forbidden surfaces as a typed non-OK status, not a success.
    assert_eq!(smoke.forbidden_status, GrpcStatusCode::PermissionDenied);
    // The cancelled call's outcome races with the fast in-process
    // server (Replied or LocalCancel), so exact equality is not pinned
    // for `cancel_outcome`; the pinned fact is that the connection
    // survives the cancel, proven inside `native_grpc_smoke` by a
    // follow-up call.
    server.shutdown().expect("shutdown specimen server");
}
