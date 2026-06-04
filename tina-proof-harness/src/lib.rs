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
//! - [`perf`]: local performance report wrapper over load/soak runs. It prints
//!   release-mode timing plus boundedness evidence for this checkout.
//!
//! The harness does not own a server. Specimens build their own service
//! and hand the harness a target (`SocketAddr` or `FnMut`). Failures are
//! returned as typed structs, never as log-scrape strings.

pub mod bad_peer;
pub mod byte_replay;
pub mod grpc;
pub mod http2;
pub mod live_replay;
pub mod load;
pub mod perf;
pub mod protocol_chaos;
pub mod websocket;

pub use bad_peer::{BadPeerOutcome, BadPeerScenario};
pub use byte_replay::{
    ByteReplayDirection, ByteReplayField, ProtocolByteReplayCase, ProtocolByteReplayIoError,
    ProtocolByteReplayMismatch, ProtocolByteReplayReport, ProtocolByteReplayShrink,
};
pub use grpc::{
    GrpcLimits, GrpcOutcome, GrpcProbe, GrpcProbeMismatch, GrpcRun, decode_grpc_response,
    grpc_probe_suite,
};
pub use http2::{
    Http2Connection, Http2Frame, Http2Limits, Http2Outcome, Http2Probe, Http2ProbeMismatch,
    http2_probe_suite,
};
pub use live_replay::{
    LiveTrace, LiveTraceHandle, LiveTraceLoss, RunCapture, RunCaptureFinishError, RunCaptureInputs,
    capture_run, replay_bug, save_bug, shrink_bug,
};
pub use load::{
    LoadAssertionFailure, LoadObservation, LoadProfile, LoadReport, LoadRun, LoadRunReport,
    LoadStop, OpOutcome, SurfacePlateau, UnavailableSurface, assert_cold_work_made_progress,
    assert_no_leaked_capacity_at_shutdown, assert_surface_plateaued_cleanly,
    assert_timer_kept_firing, cold_work_made_progress, no_leaked_capacity_at_shutdown,
    surface_plateaued_cleanly, timer_kept_firing,
};
pub use perf::{
    HotPathCounters, HotPathReport, HotPathStage, PerfAllocationReport, PerfComparisonReport,
    PerfEnvironment, PerfReport, SemanticMatch,
};
pub use protocol_chaos::{
    ChaosField, PeerAction, ProtocolChaosCase, ProtocolChaosExpectation, ProtocolChaosFamily,
    ProtocolChaosMismatch, ProtocolChaosReport, ProtocolCloseStatus, TerminalAction,
    protocol_fact_sequence_hash,
};
pub use websocket::{
    AppMessage, WebSocketComplianceCase, WebSocketComplianceMismatch, WebSocketLimits,
    WebSocketRole, WebSocketRun, WebSocketSession, compliance_corpus,
};
