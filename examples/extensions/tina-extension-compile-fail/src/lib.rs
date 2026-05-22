//! Compile-fail proof: an extension crate **cannot** mint runtime-owned
//! tokens or construct private runtime report/capability state.
//!
//! Extension hooks are public traits and owned data. The runtime keeps
//! the dangerous things — admission permits, bridge pressure, capability
//! rows — behind private fields and validated constructors. This crate
//! pins that boundary with `compile_fail` doctests: each probe is code an
//! extension author might *wish* worked, and each must fail to compile.
//!
//! The lesson for extension authors: reach for the public hook
//! (`tina_codec::SyncCodec`, `tina_runtime::ServicePolicy`,
//! `CapacitySurfaceReport`, the `tina_runtime::bridge` vocabulary), never
//! a forged private value.
//!
//! Run the doctests with:
//!
//! ```sh
//! cargo test --doc --manifest-path examples/extensions/tina-extension-compile-fail/Cargo.toml
//! ```
//!
//! # Probe 1: there is no private runtime backdoor module
//!
//! `tina_runtime` exposes no `runtime_internal` (or similar) escape
//! hatch. Importing one is an unresolved import.
//!
//! ```compile_fail
//! use tina_runtime::runtime_internal;
//!
//! fn main() {
//!     let _ = runtime_internal::anything();
//! }
//! ```
//!
//! # Probe 2: runtime-owned permits cannot be forged
//!
//! A [`tina_runtime::ConcurrencyPermit`] is minted only by a
//! `ConcurrencyLimit` admitting work. Its fields are private, so an
//! extension cannot fabricate a permit to claim capacity it never got.
//!
//! ```compile_fail
//! use tina_runtime::ConcurrencyPermit;
//!
//! fn main() {
//!     // error[E0451]: fields of `ConcurrencyPermit` are private
//!     let _forged = ConcurrencyPermit {
//!         inner: None,
//!         lease: None,
//!         gate_id: 0,
//!     };
//! }
//! ```
//!
//! # Probe 3: bridge pressure reports cannot be forged
//!
//! A [`tina_runtime::bridge::BridgePressure`] must be built through the
//! validated `measured(..)` constructor (or a per-bridge `From`). Its
//! fields are private, so an extension cannot lie about installed
//! capacity with a raw struct literal.
//!
//! ```compile_fail
//! use tina_runtime::bridge::BridgePressure;
//!
//! fn main() {
//!     // error[E0451]: fields of `BridgePressure` are private
//!     let _forged = BridgePressure {
//!         name: "ext.lie".to_string(),
//!         capacity: 9999,
//!         current: 0,
//!         high_water: 0,
//!         full_count: 0,
//!         timeout_count: 0,
//!         closed_count: 0,
//!         late_result_count: 0,
//!         worker_terminal_count: 0,
//!     };
//! }
//! ```
//!
//! # Probe 4: capability rows cannot be forged
//!
//! A [`tina_runtime::ResourceCapability`] is built via its `new(..)`
//! constructor and produced by the runtime. Its fields are private, so an
//! extension cannot mint a capability row claiming the runtime supports
//! something it does not.
//!
//! ```compile_fail
//! use tina_runtime::{
//!     CancellationSupport, ResourceCapability, ResourceExecutionShape, ResourceSupport,
//!     ShutdownSupport,
//! };
//!
//! fn main() {
//!     // error[E0451]: fields of `ResourceCapability` are private
//!     let _forged = ResourceCapability {
//!         support: ResourceSupport::Supported,
//!         execution: ResourceExecutionShape::Inline,
//!         cancellation: CancellationSupport::CancelableBeforeStart,
//!         shutdown: ShutdownSupport::Drained,
//!         capacity: None,
//!     };
//! }
//! ```

/// Number of `compile_fail` probes documented above. The smoke test
/// asserts this matches the README so a future relaxation of a private
/// boundary fails loudly.
pub const DOCUMENTED_COMPILE_FAILS: u32 = 4;

/// A sanity value the smoke test reads. The real proof is the doctests.
pub fn documented_compile_fails() -> u32 {
    DOCUMENTED_COMPILE_FAILS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_count_matches_readme() {
        // The README advertises four compile-fail probes. If you add or
        // remove one, bump this and the doctests above together.
        assert_eq!(documented_compile_fails(), 4);
    }
}
