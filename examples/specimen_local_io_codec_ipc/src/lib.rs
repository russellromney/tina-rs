//! Three side-by-side specimens for Phase 117 local I/O / codec / IPC parity:
//!
//! - [`file_ingest`] — bounded file streaming via the new
//!   `tina_runtime::FileReadChunks` and `FileWriteAll` helpers.
//! - [`admin_socket`] — local admin sidecar over a Unix-domain socket
//!   pair in the simulator with line-delimited commands from
//!   `tina_codec::LineFramer`.
//! - [`framed_keyspace`] — length-prefixed mini-keyspace protocol over
//!   Unix-domain sockets in the simulator using
//!   `tina_codec::LengthDelimitedFramer`.
//!
//! The simulator backs the IPC specimens because the live Unix-domain
//! driver returns typed `Unsupported` in this slice (see Phase 117
//! plan: "honest deferrals"). Live Unix support is named explicitly
//! in [crate::live_unix_unsupported_smoke] so non-Unix CI sees the
//! same typed result.

pub mod admin_socket;
pub mod file_ingest;
pub mod framed_keyspace;
pub mod live_unix_unsupported_smoke;

/// What a specimen run accomplished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecimenReport {
    /// Human-readable name of the run.
    pub name: &'static str,
    /// Total bytes the specimen moved through Tina rails.
    pub bytes: u64,
    /// Frames or chunks processed.
    pub frames: u64,
    /// Whether the run finished cleanly.
    pub ok: bool,
    /// Free-form note about what was demonstrated (cap reached, EOF,
    /// peer closed, etc.).
    pub note: String,
}
