//! Adversarial probe: try to smuggle shared mutable state through
//! every Tina channel.
//!
//! Categories attempted:
//!
//! 1. Send a non-`Send` value (`Rc<RefCell<T>>`) through a message.
//! 2. Move `&mut self.value` into a runtime-call reply closure.
//! 3. Stash an isolate's `&mut Self` reference inside a captured
//!    address.
//! 4. Capture an isolate-internal mutable reference inside an
//!    `Effect::Spawn` child definition.
//!
//! 1–4 do **not compile**. The README quotes the error each one
//! produces. The compile_fail doc-tests in `tina_impl::compile_fail`
//! make the test suite fail loudly if a future Tina release
//! accidentally relaxes those bounds.
//!
//! 5. Pass an `Arc<Mutex<T>>` through a message. This is the one
//!    *runtime* shape that "works": both shared and mutable state
//!    travel through the type system, but only because the user
//!    explicitly opted out of isolate-owned state. The smoke test
//!    proves it works and the README labels it as the anti-pattern.

pub mod tina_impl;

/// What the runtime probe observed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Number of compile-fail probes documented in `tina_impl`. The
    /// smoke test asserts this count matches what the README claims.
    pub documented_compile_fails: u32,
    /// `Arc<Mutex<u64>>` writes the user-side anti-pattern path made
    /// from inside the isolate. Non-zero means the type system did
    /// not block it (and we did not expect it to).
    pub intentional_escape_writes: u32,
    /// True when the runtime probe finished cleanly.
    pub exit_clean: bool,
}

/// Number of `compile_fail` doctests in `tina_impl::compile_fail`.
/// Bump this if you add another adversarial probe; the smoke test
/// compares.
pub const DOCUMENTED_COMPILE_FAILS: u32 = 4;

/// Number of writes the runtime probe makes through the
/// intentionally-escaped `Arc<Mutex<u64>>`.
pub const INTENTIONAL_ESCAPE_WRITES: u32 = 3;
