//! Shared identifier and data types for runtime-owned calls.
//!
//! Owns the newtype IDs (`CallId`, `ListenerId`, `StreamId`,
//! `UdpSocketId`, `TlsStreamId`, `TlsListenerId`, `FileId`) plus the
//! file / process / path / persistence record shapes. All these types
//! are referenced by both `CallInput` and `CallOutput` so they live
//! here instead of inside any one rail submodule.

/// Stable identifier for one runtime-issued call.
///
/// The runtime assigns `CallId`s in submission order, starting at `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallId(u64);

impl CallId {
    /// Creates a call identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw call identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a TCP listener resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListenerId(u64);

impl ListenerId {
    /// Creates a listener identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw listener identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a TCP stream resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(u64);

impl StreamId {
    /// Creates a stream identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw stream identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a UDP socket resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UdpSocketId(u64);

impl UdpSocketId {
    /// Creates a UDP socket identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw UDP socket identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a TLS stream resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TlsStreamId(u64);

impl TlsStreamId {
    /// Creates a TLS stream identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw TLS stream identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for a TLS listener resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TlsListenerId(u64);

impl TlsListenerId {
    /// Creates a TLS listener identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw listener identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned identifier for an opened file resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(u64);

impl FileId {
    /// Creates a file identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw file identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime-owned file open flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FileOpenOptions {
    /// Open the file for reading.
    pub read: bool,
    /// Open the file for writing.
    pub write: bool,
    /// Create the file if it does not already exist.
    pub create: bool,
    /// Truncate the file to zero length on open.
    pub truncate: bool,
}

/// Exit status for a bounded local process run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessStatus {
    /// Portable process exit code when the platform exposes one.
    pub code: Option<i32>,
}

/// Result of a bounded local process run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRunResult {
    /// Process exit status.
    pub status: ProcessStatus,
    /// Captured stdout, capped by request.
    pub stdout: Vec<u8>,
    /// Captured stderr, capped by request.
    pub stderr: Vec<u8>,
    /// Whether stdout exceeded its cap.
    pub stdout_truncated: bool,
    /// Whether stderr exceeded its cap.
    pub stderr_truncated: bool,
}

/// Coarse kind of one local filesystem path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathKind {
    /// The path does not exist.
    Missing,

    /// The path is a regular file.
    File,

    /// The path is a directory.
    Directory,

    /// The path exists, but is neither a regular file nor a directory.
    Other,
}

/// Metadata Tina exposes for a local filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathMetadata {
    /// Coarse path kind.
    pub kind: PathKind,

    /// File length when the path is a regular file.
    pub len: Option<u64>,
}

impl PathMetadata {
    /// Returns metadata for a missing path.
    pub const fn missing() -> Self {
        Self {
            kind: PathKind::Missing,
            len: None,
        }
    }

    /// Returns whether the path exists.
    pub const fn exists(&self) -> bool {
        !matches!(self.kind, PathKind::Missing)
    }
}

impl FileOpenOptions {
    /// Opens an existing file for reading only.
    pub const fn read_only() -> Self {
        Self {
            read: true,
            write: false,
            create: false,
            truncate: false,
        }
    }

    /// Opens an existing file for writing only.
    pub const fn write_only() -> Self {
        Self {
            read: false,
            write: true,
            create: false,
            truncate: false,
        }
    }

    /// Opens an existing file for reading and writing.
    pub const fn read_write() -> Self {
        Self {
            read: true,
            write: true,
            create: false,
            truncate: false,
        }
    }

    /// Opens a file for reading and writing, creating it if needed and
    /// truncating any existing contents.
    pub const fn read_write_create_truncate() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            truncate: true,
        }
    }
}

/// Durable snapshot loaded by Tina's local persistence helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotImage {
    /// Opaque user payload.
    pub bytes: Vec<u8>,
    /// Last journal record reflected by this snapshot.
    pub last_journal_index: u64,
}

/// One durable domain record decoded from a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// Monotonic application-owned record index.
    pub index: u64,
    /// Opaque user payload.
    pub bytes: Vec<u8>,
}

/// Non-fatal journal replay warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalReplayWarning {
    /// The journal ended with an incomplete final record. Valid prefix records
    /// were returned. `valid_prefix_len` is the byte length that can be
    /// safely retained before appending new records.
    TruncatedTail {
        /// Number of valid prefix bytes in the journal file.
        valid_prefix_len: u64,
    },
}

/// Journal replay output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalReplay {
    /// Valid records in durable order.
    pub records: Vec<JournalRecord>,
    /// Warning for a non-fatal tail condition.
    pub warning: Option<JournalReplayWarning>,
}
