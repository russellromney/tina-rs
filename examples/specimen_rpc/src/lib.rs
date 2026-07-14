//! Tina-vs-Tokio RPC overload comparison.
//!
//! Same workload, two implementations. Read [`tokio_impl`] and
//! [`tina_impl`] top-to-bottom; the README compares feel.

pub mod tina_impl;
pub mod tokio_impl;

/// Burst size each side fires from one client connection.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub burst: usize,
}

pub const MAX_BURST: usize = 4_096;
pub const MAX_REQUEST_BYTES: usize = 1_048_576;

impl RunConfig {
    pub fn validate(self) -> anyhow::Result<Self> {
        if self.burst == 0 {
            anyhow::bail!("burst must be greater than zero");
        }
        if self.burst > MAX_BURST {
            anyhow::bail!("burst {} exceeds maximum {MAX_BURST}", self.burst);
        }
        Ok(self)
    }
}

impl Default for RunConfig {
    fn default() -> Self {
        Self { burst: 4 }
    }
}

/// What each side observed on the wire. `ok` = `Reply` frames.
/// `full` = wire `Error(Full)` frames. `other` covers anything
/// unexpected (decode error, connection close mid-burst) so totals
/// never silently shrink.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub ok: usize,
    pub full: usize,
    pub other: usize,
    pub wire_errors: WireErrorCounts,
    pub client_terminal: Option<ClientTerminal>,
    pub decode_errors: Vec<tina_rpc::DecodeError>,
    pub unexpected_frames: Vec<UnexpectedFrame>,
    pub listener_terminal: Option<ListenerTerminal>,
    pub connection_terminal: Option<tina_rpc::CloseReason>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WireErrorCounts {
    pub full: usize,
    pub unknown_service: usize,
    pub unknown_method: usize,
    pub decode: usize,
    pub protocol: usize,
    pub internal: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientTerminal {
    Eof,
    Read(std::io::ErrorKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerTerminal {
    ClosedClean,
    BindFailed(tina_runtime::CallError),
    AcceptFailed(tina_runtime::CallError),
    CloseFailed(tina_runtime::CallError),
    MissingListener,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnexpectedFrame {
    pub kind: tina_rpc::FrameKind,
    pub error: Option<tina_rpc::FrameError>,
}

impl Report {
    pub fn total(&self) -> usize {
        self.ok + self.full + self.other
    }
}
