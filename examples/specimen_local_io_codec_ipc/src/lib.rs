//! Three side-by-side specimens for the local I/O, codec, and IPC parity specimen local I/O / codec / IPC parity:
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
//! The IPC specimens run on the deterministic simulator so the framed
//! protocol logic is replayable. The live OS-backed Unix-domain rail is
//! exercised separately by [crate::live_unix_smoke], which drives the
//! real runtime — binding a true socket on Unix, typed `Unsupported`
//! off Unix.

pub mod admin_socket;
pub mod file_ingest;
pub mod framed_keyspace;
pub mod live_unix_smoke;

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
