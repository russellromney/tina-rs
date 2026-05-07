#![feature(allocator_api)]

//! Tiny CI smoke for the native Betelgeuse backend.
//!
//! Betelgeuse owns the platform backend. Tina's job here is just to make the
//! selected backend visible and keep a small Linux/macOS check in CI.

use std::alloc::Global;

use betelgeuse::{IO, IOLoop, io_loop};

#[test]
fn native_betelgeuse_backend_name_matches_platform() {
    let io_loop = io_loop(Global).expect("native Betelgeuse I/O loop starts");
    let backend = io_loop.backend_name();

    #[cfg(target_os = "linux")]
    assert_eq!(backend, "linux");

    #[cfg(target_os = "macos")]
    assert_eq!(backend, "darwin");

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    assert_eq!(backend, "unsupported");
}

#[test]
fn native_betelgeuse_loop_can_step_when_idle() {
    let io_loop = io_loop(Global).expect("native Betelgeuse I/O loop starts");

    assert!(
        !io_loop.step().expect("idle native Betelgeuse step works"),
        "idle native backend should report no progress"
    );
}
