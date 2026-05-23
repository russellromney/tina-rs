//! Small proof harnesses for Tina system specimens.
//!
//! Three pieces, each tiny on purpose:
//!
//! - [`load`]: concurrent op driver with typed latency + leak summary.
//! - [`bad_peer`]: reusable bad TCP/HTTP clients (half-close, RST,
//!   slowloris, stalled writer, stalled reader, malformed frame, reconnect
//!   storm). Each scenario returns a typed [`bad_peer::BadPeerOutcome`].
//! - [`live_replay`]: small wrapper over `tina-runtime`'s `TraceObserver`
//!   that captures live events into a `Vec<RuntimeEvent>` and computes a
//!   [`tina_sim::dst::TraceShape`] so the saved shape can be compared
//!   against a `ReplayCase` in the simulator.
//!
//! The harness does not own a server. Specimens build their own service
//! and hand the harness a target (`SocketAddr` or `FnMut`). Failures are
//! returned as typed structs, never as log-scrape strings.

pub mod bad_peer;
pub mod live_replay;
pub mod load;

pub use bad_peer::{BadPeerOutcome, BadPeerScenario};
pub use live_replay::{LiveTrace, LiveTraceHandle, LiveTraceLoss};
pub use load::{
    LoadObservation, LoadProfile, LoadReport, LoadRun, LoadRunReport, LoadStop, OpOutcome,
    SurfacePlateau, UnavailableSurface,
};
