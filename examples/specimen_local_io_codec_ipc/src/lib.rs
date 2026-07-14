//! Three side-by-side specimens for local I/O, codec, and IPC parity:
//!
//! - [`file_ingest`] — bounded file streaming via
//!   `tina_runtime::FileReadChunks` and `FileWriteAll` helpers.
//! - [`admin_socket`] — local admin sidecar over simulator Unix-domain
//!   sockets with line-delimited decode and bounded framed output.
//! - [`framed_keyspace`] — length-prefixed mini-keyspace protocol over
//!   simulator Unix-domain sockets with bounded decode and output.
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

use std::time::Duration;

use tina_runtime::{IngressSendError, IsolateResultWaiter, ResultWaitError};

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

/// Why a bounded host-side start send was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// The address belonged to another simulator/system incarnation.
    ForeignSystem {
        /// Incarnation owned by the host.
        expected: tina::SystemIncarnation,
        /// Incarnation carried by the address.
        actual: tina::SystemIncarnation,
    },
    /// The actor mailbox was at capacity.
    Full,
    /// The actor mailbox was closed.
    Closed,
}

/// Failure shared by the simulator-backed specimen runners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError<E> {
    /// A caller supplied configuration that cannot be represented safely.
    InvalidConfig(&'static str),
    /// Typed terminal-result authority could not be registered or resolved.
    Observe {
        /// Actor whose result was being observed.
        actor: &'static str,
        /// Exact observation failure.
        error: ResultWaitError,
    },
    /// The bounded start message was refused.
    Start {
        /// Actor that could not be started.
        actor: &'static str,
        /// Exact mailbox outcome.
        error: StartError,
    },
    /// An actor returned a typed terminal failure.
    Actor {
        /// Actor that failed.
        actor: &'static str,
        /// Exact actor failure.
        error: E,
    },
    /// The simulator quiesced while rail authority was still outstanding.
    InFlightCalls,
}

impl<E: std::fmt::Display> std::fmt::Display for RunError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::Observe { actor, error } => {
                write!(formatter, "could not observe {actor}: {error:?}")
            }
            Self::Start { actor, error } => {
                write!(formatter, "could not start {actor}: {error:?}")
            }
            Self::Actor { actor, error } => write!(formatter, "{actor} failed: {error}"),
            Self::InFlightCalls => {
                formatter.write_str("simulator quiesced with calls still in flight")
            }
        }
    }
}

impl<E> std::error::Error for RunError<E> where E: std::fmt::Debug + std::fmt::Display {}

pub(crate) fn map_start<T, E>(
    actor: &'static str,
    result: Result<(), IngressSendError<T>>,
) -> Result<(), RunError<E>> {
    result.map_err(|error| RunError::Start {
        actor,
        error: match error {
            IngressSendError::ForeignSystem {
                expected, actual, ..
            } => StartError::ForeignSystem { expected, actual },
            IngressSendError::Full(_) => StartError::Full,
            IngressSendError::Closed(_) => StartError::Closed,
        },
    })
}

pub(crate) fn wait_actor<T, E>(
    actor: &'static str,
    waiter: IsolateResultWaiter<Result<T, E>>,
    timeout: Duration,
) -> Result<T, RunError<E>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    waiter
        .wait(timeout)
        .map_err(|error| RunError::Observe { actor, error })?
        .map_err(|error| RunError::Actor { actor, error })
}
