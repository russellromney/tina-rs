//! Runtime-owned external call vocabulary for `tina-runtime-current`.
//!
//! `tina` only owns the [`Effect::Call`](tina::Effect::Call) slot and the
//! `Isolate::Call` associated type. The concrete request and result types
//! live here so the trait crate stays substrate-neutral. A future
//! Mariner runtime crate (a different completion-driven backend, a
//! deterministic simulator, …) can implement the same `Effect::Call`
//! slot with its own request/result vocabulary without touching `tina`.
//!
//! The first shipped call family is backed by Betelgeuse on nightly Rust and
//! now covers both:
//!
//! - TCP bind / accept / read / write / close
//! - one-shot relative sleep / timer wake
//!
//! Future verbs still extend [`CallRequest`] / [`CallResult`] in this crate,
//! not the `tina` trait boundary.
//!
//! ## Design constraints honored here
//!
//! - Resource ids ([`ListenerId`], [`StreamId`], [`CallId`]) are
//!   runtime-assigned monotonic counters. The runtime, not the OS, owns
//!   identity.
//! - No wall-clock time leaks into the call payload. The call family
//!   names relative timer duration, not `Instant`, so a deterministic
//!   simulator can implement virtual time without renegotiating the
//!   contract.
//! - Raw socket handles never escape the runtime. Isolate code only sees
//!   opaque ids inside its own message vocabulary.

use std::any::Any;
use std::net::SocketAddr;
use std::time::Duration;

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

/// One concrete call shape understood by `tina-runtime-current`.
///
/// New verbs are added by extending this enum, not by adding a top-level
/// [`tina::Effect`] variant per verb.
#[derive(Debug, Clone)]
pub enum CallRequest {
    /// Bind a TCP listener to `addr`.
    ///
    /// Uses [`SocketAddr`] rather than a logical name. The runtime reports the
    /// actual bound address back through [`CallResult::TcpBound::local_addr`],
    /// including when the caller requests port `0` and lets the kernel pick a
    /// free ephemeral port.
    TcpBind {
        /// The address the listener should bind to.
        addr: SocketAddr,
    },

    /// Accept one inbound connection on a previously-bound listener.
    TcpAccept {
        /// The listener to accept on.
        listener: ListenerId,
    },

    /// Read up to `max_len` bytes from a stream.
    ///
    /// A successful read of zero bytes signals end of stream; the runtime
    /// surfaces that in [`CallResult::TcpRead`] with an empty `bytes`
    /// vector.
    TcpRead {
        /// The stream to read from.
        stream: StreamId,

        /// The maximum number of bytes the runtime may deliver in this
        /// completion.
        max_len: usize,
    },

    /// Write `bytes` to a stream. Partial writes are surfaced through
    /// [`CallResult::TcpWrote`] so the issuing isolate can decide whether
    /// to issue another write for the remaining bytes.
    TcpWrite {
        /// The stream to write to.
        stream: StreamId,

        /// The payload to write.
        bytes: Vec<u8>,
    },

    /// Close a previously-bound listener and release its resources.
    TcpListenerClose {
        /// The listener to close.
        listener: ListenerId,
    },

    /// Close a previously-accepted stream and release its resources.
    TcpStreamClose {
        /// The stream to close.
        stream: StreamId,
    },

    /// Sleep for a relative duration.
    ///
    /// Completion fires no earlier than `armed_at + after` on a future
    /// step. The runtime samples its monotonic clock once per step;
    /// timers due at or before that sampled instant become eligible in
    /// that step.
    Sleep {
        /// The duration to wait before waking.
        after: Duration,
    },
}

impl CallRequest {
    /// Returns the trace-level kind for this request.
    pub(crate) fn kind(&self) -> crate::trace::CallKind {
        match self {
            Self::TcpBind { .. } => crate::trace::CallKind::TcpBind,
            Self::TcpAccept { .. } => crate::trace::CallKind::TcpAccept,
            Self::TcpRead { .. } => crate::trace::CallKind::TcpRead,
            Self::TcpWrite { .. } => crate::trace::CallKind::TcpWrite,
            Self::TcpListenerClose { .. } => crate::trace::CallKind::TcpListenerClose,
            Self::TcpStreamClose { .. } => crate::trace::CallKind::TcpStreamClose,
            Self::Sleep { .. } => crate::trace::CallKind::Sleep,
        }
    }
}

/// One concrete call completion delivered to the issuing isolate.
#[derive(Debug, Clone)]
pub enum CallResult {
    /// A listener was successfully bound and is ready to accept.
    TcpBound {
        /// The runtime-assigned listener identifier.
        listener: ListenerId,

        /// The actual bound address reported by the runtime's I/O substrate.
        local_addr: SocketAddr,
    },

    /// A connection was accepted on a listener.
    TcpAccepted {
        /// The runtime-assigned stream identifier for the new connection.
        stream: StreamId,

        /// The remote peer address of the accepted stream.
        peer_addr: SocketAddr,
    },

    /// A read returned `bytes`. An empty `bytes` vector means end of stream.
    TcpRead {
        /// The bytes the runtime read from the stream.
        bytes: Vec<u8>,
    },

    /// A write moved `count` bytes onto the stream. The issuing isolate is
    /// responsible for issuing another write if `count` is less than the
    /// requested length.
    TcpWrote {
        /// The number of bytes the runtime accepted.
        count: usize,
    },

    /// A listener was closed and its resources released.
    TcpListenerClosed,

    /// A stream was closed and its resources released.
    TcpStreamClosed,

    /// A timer sleep completed and the isolate should wake.
    TimerFired,

    /// The runtime could not complete the call. The trace already records
    /// the failure with a richer reason; this variant is what the issuing
    /// isolate observes in its own vocabulary.
    Failed(CallFailureReason),
}

/// Why a runtime-owned call failed before it could complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallFailureReason {
    /// The referenced listener or stream id is not registered with the
    /// runtime.
    InvalidResource,

    /// The runtime's underlying I/O substrate returned an error. The trace
    /// also records this; the isolate-facing variant is intentionally
    /// opaque to keep `os::ErrorKind` out of the call contract.
    Io,

    /// The runtime cannot honestly perform the requested operation on
    /// this substrate revision.
    ///
    /// Reserved for capability gaps the runtime cannot honestly
    /// perform on the current substrate revision. Future gaps should
    /// surface here rather than through silent fallbacks.
    Unsupported,
}

/// Backend-neutral runtime-owned call request issued by an isolate.
///
/// The translator turns the runtime's later [`CallResult`] back into one
/// ordinary [`tina::Isolate::Message`] value. The runtime never invokes a
/// second public handler entry point — completion always travels through
/// the isolate's existing [`Isolate::handle`](tina::Isolate::handle).
///
/// The struct is not [`Clone`] on purpose: a call request is meant to be
/// moved into the runtime, not duplicated, and the translator boxes a
/// non-`Clone` `FnOnce`.
#[must_use = "a call request has no effect until a runtime executes it"]
pub struct CurrentCall<M> {
    request: CallRequest,
    translator: Box<dyn FnOnce(CallResult) -> M>,
}

impl<M> CurrentCall<M> {
    /// Creates a new runtime-owned call request.
    ///
    /// `translator` runs once, when the runtime delivers the call's
    /// completion back to the issuing isolate. It must produce exactly one
    /// `Message` value — this is the load-bearing rule that keeps the
    /// completion path "ordinary later message," not a second handler
    /// entry point.
    pub fn new<F>(request: CallRequest, translator: F) -> Self
    where
        F: FnOnce(CallResult) -> M + 'static,
    {
        Self {
            request,
            translator: Box::new(translator),
        }
    }

    /// Returns a shared reference to the underlying request.
    pub fn request(&self) -> &CallRequest {
        &self.request
    }

    /// Splits the call into its request and translator.
    pub fn into_parts(self) -> (CallRequest, Box<dyn FnOnce(CallResult) -> M>) {
        (self.request, self.translator)
    }
}

impl<M> std::fmt::Debug for CurrentCall<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CurrentCall")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// Erased call shape stored by the runtime once an isolate's
/// per-`I::Message` translator has been wrapped to a `Box<dyn Any>`.
///
/// `tina-runtime-current` does not expose this type's fields. Downstream
/// crates produce `ErasedCall` only via the [`IntoErasedCall`] conversion
/// trait. The struct is exposed publicly only to make the trait method
/// signature visible; it is not constructible from outside this crate.
pub struct ErasedCall {
    pub(crate) request: CallRequest,
    pub(crate) translator: Box<dyn FnOnce(CallResult) -> Box<dyn Any>>,
}

impl std::fmt::Debug for ErasedCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ErasedCall")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// Conversion from one isolate-level call payload into the runtime's
/// erased form.
///
/// This mirrors the existing `IntoErasedSpawn` pattern: isolates that never
/// issue call effects use [`std::convert::Infallible`], and isolates that
/// do use `CurrentCall<I::Message>`. New runtime crates that want a
/// different programming model implement their own conversion trait —
/// `tina` does not pin the conversion shape.
pub trait IntoErasedCall<M> {
    /// Erases the call into the runtime's internal form.
    fn into_erased_call(self) -> ErasedCall;
}

impl<M> IntoErasedCall<M> for std::convert::Infallible {
    fn into_erased_call(self) -> ErasedCall {
        match self {}
    }
}

impl<M> IntoErasedCall<M> for CurrentCall<M>
where
    M: 'static,
{
    fn into_erased_call(self) -> ErasedCall {
        let (request, translator) = self.into_parts();
        ErasedCall {
            request,
            translator: Box::new(move |result| Box::new(translator(result)) as Box<dyn Any>),
        }
    }
}
