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

impl Default for RunConfig {
    fn default() -> Self {
        Self { burst: 4 }
    }
}

/// What each side observed on the wire. `ok` = `Reply` frames.
/// `full` = wire `Error(Full)` frames. `other` covers anything
/// unexpected (decode error, connection close mid-burst) so totals
/// never silently shrink.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub ok: usize,
    pub full: usize,
    pub other: usize,
}

impl Report {
    pub fn total(&self) -> usize {
        self.ok + self.full + self.other
    }
}
