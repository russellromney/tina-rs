//! Runtime-owned external call vocabulary for `tina-runtime`.
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
//! Future verbs still extend [`CallInput`] / [`CallOutput`] in this crate,
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

use tina::{Address, AddressGeneration, IsolateId, ShardId};

type ErasedReply = Box<dyn Any>;
type ErasedCallOutcome = CallOutcome<ErasedReply>;
type IsolateCallTranslator<M> = Box<dyn FnOnce(ErasedCallOutcome) -> M>;
type ErasedIsolateCallTranslator = Box<dyn FnOnce(ErasedCallOutcome) -> Box<dyn Any>>;

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

/// One concrete call shape understood by `tina-runtime`.
///
/// New verbs are added by extending this enum, not by adding a top-level
/// [`tina::Effect`] variant per verb.
#[derive(Debug, Clone)]
pub enum CallInput {
    /// Bind a TCP listener to `addr`.
    ///
    /// Uses [`SocketAddr`] rather than a logical name. The runtime reports the
    /// actual bound address back through [`CallOutput::TcpBound::local_addr`],
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
    /// surfaces that in [`CallOutput::TcpRead`] with an empty `bytes`
    /// vector.
    TcpRead {
        /// The stream to read from.
        stream: StreamId,

        /// The maximum number of bytes the runtime may deliver in this
        /// completion.
        max_len: usize,
    },

    /// Write `bytes` to a stream. Partial writes are surfaced through
    /// [`CallOutput::TcpWrote`] so the issuing isolate can decide whether
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

impl CallInput {
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
pub enum CallOutput {
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
    Failed(CallError),
}

/// Why a runtime-owned call failed before it could complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallError {
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

    /// The target isolate's mailbox was full when the runtime attempted an
    /// isolate-to-isolate call.
    TargetFull,

    /// The target isolate was closed, stale, or otherwise unavailable when
    /// the runtime attempted an isolate-to-isolate call.
    TargetClosed,

    /// The target isolate did not reply before the caller's timeout elapsed.
    Timeout,
}

/// User-visible outcome for an observed send.
///
/// Ordinary [`tina::send`] stays fire-and-forget. This outcome is only
/// produced by [`send_observed`], for code that needs explicit overload
/// policy in normal isolate message flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SendOutcome {
    /// The runtime accepted the send into the destination mailbox or, for a
    /// remote shard, into the bounded transport toward that shard.
    Accepted,

    /// The destination mailbox or bounded transport was full.
    Full,

    /// The destination was closed, stale, or otherwise no longer live.
    Closed,
}

/// User-visible outcome for an isolate-to-isolate call.
///
/// A call is just a bounded message send plus one later reply. The timeout is
/// mandatory so callers cannot accidentally create invisible forever-waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome<T> {
    /// The target isolate replied with a value of the expected type.
    Replied(T),

    /// The target isolate's mailbox was full.
    Full,

    /// The target isolate was closed, stale, or otherwise unavailable.
    Closed,

    /// The target isolate did not reply before the timeout elapsed.
    Timeout,
}

impl SendOutcome {
    pub(crate) fn from_rejected(reason: crate::trace::SendRejectedReason) -> Self {
        match reason {
            crate::trace::SendRejectedReason::Full => Self::Full,
            crate::trace::SendRejectedReason::Closed => Self::Closed,
        }
    }
}

/// Backend-neutral runtime-owned call request issued by an isolate.
///
/// The translator turns the runtime's later [`CallOutput`] back into one
/// ordinary [`tina::Isolate::Message`] value. The runtime never invokes a
/// second public handler entry point — completion always travels through
/// the isolate's existing [`Isolate::handle`](tina::Isolate::handle).
///
/// The struct is not [`Clone`] on purpose: a call request is meant to be
/// moved into the runtime, not duplicated, and the translator boxes a
/// non-`Clone` `FnOnce`.
#[must_use = "a call request has no effect until a runtime executes it"]
pub struct RuntimeCall<M> {
    kind: RuntimeCallKind<M>,
}

enum RuntimeCallKind<M> {
    Backend {
        request: CallInput,
        translator: Box<dyn FnOnce(CallOutput) -> M>,
    },
    ObservedSend {
        target_shard: ShardId,
        target_isolate: IsolateId,
        target_generation: AddressGeneration,
        message: Box<dyn Any + Send>,
        translator: Box<dyn FnOnce(SendOutcome) -> M>,
    },
    IsolateCall {
        target_shard: ShardId,
        target_isolate: IsolateId,
        target_generation: AddressGeneration,
        message: Box<dyn Any + Send>,
        timeout: Duration,
        translator: IsolateCallTranslator<M>,
    },
}

impl<M> RuntimeCall<M> {
    /// Creates a new runtime-owned call request.
    ///
    /// `translator` runs once, when the runtime delivers the call's
    /// completion back to the issuing isolate. It must produce exactly one
    /// `Message` value — this is the load-bearing rule that keeps the
    /// completion path "ordinary later message," not a second handler
    /// entry point.
    pub fn new<F>(request: CallInput, translator: F) -> Self
    where
        F: FnOnce(CallOutput) -> M + 'static,
    {
        Self {
            kind: RuntimeCallKind::Backend {
                request,
                translator: Box::new(translator),
            },
        }
    }

    /// Creates a runtime-observed send request.
    ///
    /// The runtime attempts the send and later delivers one [`SendOutcome`]
    /// through `translator`. This preserves Tina's effect-returning handler
    /// model: the current handler turn still returns a description of work,
    /// and overload feedback comes back as an ordinary later message.
    pub fn observed_send<T, F>(destination: Address<T>, message: T, translator: F) -> Self
    where
        T: Send + 'static,
        F: FnOnce(SendOutcome) -> M + 'static,
    {
        Self {
            kind: RuntimeCallKind::ObservedSend {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: Box::new(message),
                translator: Box::new(translator),
            },
        }
    }

    /// Creates an isolate-to-isolate call request.
    ///
    /// The destination receives `message` as an ordinary handler message. If
    /// that handler later returns [`tina::reply`], the reply becomes
    /// [`CallOutcome::Replied`] for the requester. The timeout is mandatory.
    pub fn isolate_call<T, R, F>(
        destination: Address<T>,
        message: T,
        timeout: Duration,
        translator: F,
    ) -> Self
    where
        T: Send + 'static,
        R: 'static,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
    {
        Self {
            kind: RuntimeCallKind::IsolateCall {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: Box::new(message),
                timeout,
                translator: Box::new(move |outcome| match outcome {
                    CallOutcome::Replied(reply) => {
                        let reply = *reply.downcast::<R>().unwrap_or_else(|_| {
                            panic!(
                                "isolate call reply had the wrong type; expected {}",
                                std::any::type_name::<R>()
                            )
                        });
                        translator(CallOutcome::Replied(reply))
                    }
                    CallOutcome::Full => translator(CallOutcome::Full),
                    CallOutcome::Closed => translator(CallOutcome::Closed),
                    CallOutcome::Timeout => translator(CallOutcome::Timeout),
                }),
            },
        }
    }

    /// Creates a call that receives a plain `Result<CallOutput, CallError>`
    /// instead of matching the failure variant manually.
    pub fn map_result<F>(request: CallInput, translator: F) -> Self
    where
        F: FnOnce(Result<CallOutput, CallError>) -> M + 'static,
    {
        Self::new(request, move |output| match output {
            CallOutput::Failed(error) => translator(Err(error)),
            other => translator(Ok(other)),
        })
    }

    /// Returns a shared reference to the underlying request.
    pub fn request(&self) -> &CallInput {
        match &self.kind {
            RuntimeCallKind::Backend { request, .. } => request,
            RuntimeCallKind::ObservedSend { .. } => {
                panic!("observed send does not carry a backend CallInput")
            }
            RuntimeCallKind::IsolateCall { .. } => {
                panic!("isolate call does not carry a backend CallInput")
            }
        }
    }

    /// Splits the call into its request and translator.
    pub fn into_parts(self) -> (CallInput, Box<dyn FnOnce(CallOutput) -> M>) {
        match self.kind {
            RuntimeCallKind::Backend {
                request,
                translator,
            } => (request, translator),
            RuntimeCallKind::ObservedSend { .. } => {
                panic!("observed send does not carry backend call parts")
            }
            RuntimeCallKind::IsolateCall { .. } => {
                panic!("isolate call does not carry backend call parts")
            }
        }
    }

    /// Splits this call into the runtime action it describes.
    ///
    /// This is primarily for sibling runtime/simulator crates that need to
    /// interpret `RuntimeCall` without depending on private fields.
    #[doc(hidden)]
    pub fn into_runtime_parts(self) -> RuntimeCallParts<M> {
        match self.kind {
            RuntimeCallKind::Backend {
                request,
                translator,
            } => RuntimeCallParts::Backend {
                request,
                translator,
            },
            RuntimeCallKind::ObservedSend {
                target_shard,
                target_isolate,
                target_generation,
                message,
                translator,
            } => RuntimeCallParts::ObservedSend {
                target_shard,
                target_isolate,
                target_generation,
                message,
                translator,
            },
            RuntimeCallKind::IsolateCall {
                target_shard,
                target_isolate,
                target_generation,
                message,
                timeout,
                translator,
            } => RuntimeCallParts::IsolateCall {
                target_shard,
                target_isolate,
                target_generation,
                message,
                timeout,
                translator,
            },
        }
    }
}

/// Publicly destructurable runtime action carried by [`RuntimeCall`].
#[doc(hidden)]
pub enum RuntimeCallParts<M> {
    /// Backend-owned I/O/time request.
    Backend {
        /// Concrete backend request.
        request: CallInput,
        /// Completion translator.
        translator: Box<dyn FnOnce(CallOutput) -> M>,
    },
    /// Runtime-observed send request.
    ObservedSend {
        /// Destination shard.
        target_shard: ShardId,
        /// Destination isolate.
        target_isolate: IsolateId,
        /// Destination generation.
        target_generation: AddressGeneration,
        /// Erased message payload.
        message: Box<dyn Any + Send>,
        /// Outcome translator.
        translator: Box<dyn FnOnce(SendOutcome) -> M>,
    },
    /// Isolate-to-isolate call request.
    IsolateCall {
        /// Destination shard.
        target_shard: ShardId,
        /// Destination isolate.
        target_isolate: IsolateId,
        /// Destination generation.
        target_generation: AddressGeneration,
        /// Erased request message payload.
        message: Box<dyn Any + Send>,
        /// Mandatory caller timeout.
        timeout: Duration,
        /// Outcome translator.
        translator: IsolateCallTranslator<M>,
    },
}

impl<M> std::fmt::Debug for RuntimeCall<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeCall")
            .field(
                "kind",
                &match &self.kind {
                    RuntimeCallKind::Backend { request, .. } => request.kind(),
                    RuntimeCallKind::ObservedSend { .. } => crate::trace::CallKind::ObservedSend,
                    RuntimeCallKind::IsolateCall { .. } => crate::trace::CallKind::IsolateCall,
                },
            )
            .finish_non_exhaustive()
    }
}

/// Erased call shape stored by the runtime once an isolate's
/// per-`I::Message` translator has been wrapped to a `Box<dyn Any>`.
///
/// `tina-runtime` does not expose this type's fields. Downstream
/// crates produce `ErasedCall` only via the [`IntoErasedCall`] conversion
/// trait. The struct is exposed publicly only to make the trait method
/// signature visible; it is not constructible from outside this crate.
pub struct ErasedCall {
    pub(crate) kind: ErasedCallKind,
}

pub(crate) enum ErasedCallKind {
    /// Runtime-owned I/O/time call executed by a backend.
    Backend {
        /// Concrete backend request.
        request: CallInput,
        /// Completion translator erased to `Any`.
        translator: Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>,
    },
    /// Runtime-observed send attempt.
    ObservedSend {
        /// Erased outbound message to attempt.
        send: crate::ErasedSend,
        /// Outcome translator erased to `Any`.
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
    },
    /// Isolate-to-isolate call request.
    IsolateCall {
        /// Erased outbound request message.
        send: crate::ErasedSend,
        /// Mandatory caller timeout.
        timeout: Duration,
        /// Outcome translator erased to `Any`.
        translator: ErasedIsolateCallTranslator,
    },
}

impl std::fmt::Debug for ErasedCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ErasedCall")
            .field(
                "kind",
                &match &self.kind {
                    ErasedCallKind::Backend { request, .. } => request.kind(),
                    ErasedCallKind::ObservedSend { .. } => crate::trace::CallKind::ObservedSend,
                    ErasedCallKind::IsolateCall { .. } => crate::trace::CallKind::IsolateCall,
                },
            )
            .finish_non_exhaustive()
    }
}

/// Conversion from one isolate-level call payload into the runtime's
/// erased form.
///
/// This mirrors the existing `IntoErasedSpawn` pattern: isolates that never
/// issue call effects use [`std::convert::Infallible`], and isolates that
/// do use `RuntimeCall<I::Message>`. New runtime crates that want a
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

impl<M> IntoErasedCall<M> for RuntimeCall<M>
where
    M: 'static,
{
    fn into_erased_call(self) -> ErasedCall {
        match self.into_runtime_parts() {
            RuntimeCallParts::Backend {
                request,
                translator,
            } => ErasedCall {
                kind: ErasedCallKind::Backend {
                    request,
                    translator: Box::new(move |result| {
                        Box::new(translator(result)) as Box<dyn Any>
                    }),
                },
            },
            RuntimeCallParts::ObservedSend {
                target_shard,
                target_isolate,
                target_generation,
                message,
                translator,
            } => ErasedCall {
                kind: ErasedCallKind::ObservedSend {
                    send: crate::ErasedSend {
                        target_shard,
                        target_isolate,
                        target_generation,
                        message: crate::ErasedMessage::Sendable(message),
                    },
                    translator: Box::new(move |outcome| {
                        Box::new(translator(outcome)) as Box<dyn Any>
                    }),
                },
            },
            RuntimeCallParts::IsolateCall {
                target_shard,
                target_isolate,
                target_generation,
                message,
                timeout,
                translator,
            } => ErasedCall {
                kind: ErasedCallKind::IsolateCall {
                    send: crate::ErasedSend {
                        target_shard,
                        target_isolate,
                        target_generation,
                        message: crate::ErasedMessage::Sendable(message),
                    },
                    timeout,
                    translator: Box::new(move |outcome| {
                        Box::new(translator(outcome)) as Box<dyn Any>
                    }),
                },
            },
        }
    }
}

impl CallOutput {
    fn panic_wrong_shape(expected: &str, found: &Self) -> ! {
        panic!("typed runtime call helper expected {expected}, but runtime returned {found:?}");
    }

    /// Extracts the timer completion payload.
    pub fn into_timer_fired(self) -> Result<(), CallError> {
        match self {
            Self::TimerFired => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TimerFired", &other),
        }
    }

    /// Extracts the successful TCP bind result.
    pub fn into_tcp_bound(self) -> Result<(ListenerId, SocketAddr), CallError> {
        match self {
            Self::TcpBound {
                listener,
                local_addr,
            } => Ok((listener, local_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpBound", &other),
        }
    }

    /// Extracts the successful TCP accept result.
    pub fn into_tcp_accepted(self) -> Result<(StreamId, SocketAddr), CallError> {
        match self {
            Self::TcpAccepted { stream, peer_addr } => Ok((stream, peer_addr)),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpAccepted", &other),
        }
    }

    /// Extracts the successful TCP read payload.
    pub fn into_tcp_read(self) -> Result<Vec<u8>, CallError> {
        match self {
            Self::TcpRead { bytes } => Ok(bytes),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpRead", &other),
        }
    }

    /// Extracts the successful TCP write payload.
    pub fn into_tcp_wrote(self) -> Result<usize, CallError> {
        match self {
            Self::TcpWrote { count } => Ok(count),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpWrote", &other),
        }
    }

    /// Extracts the successful listener close completion.
    pub fn into_tcp_listener_closed(self) -> Result<(), CallError> {
        match self {
            Self::TcpListenerClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpListenerClosed", &other),
        }
    }

    /// Extracts the successful stream close completion.
    pub fn into_tcp_stream_closed(self) -> Result<(), CallError> {
        match self {
            Self::TcpStreamClosed => Ok(()),
            Self::Failed(error) => Err(error),
            other => Self::panic_wrong_shape("TcpStreamClosed", &other),
        }
    }
}

/// Doc-hidden carrier used by typed call helpers like [`sleep`] and
/// [`tcp_read`].
#[doc(hidden)]
pub struct TypedCall<T> {
    request: CallInput,
    decode: fn(CallOutput) -> Result<T, CallError>,
}

/// Prepared observed-send helper returned by [`send_observed`].
#[doc(hidden)]
pub struct ObservedSend<T> {
    destination: Address<T>,
    message: T,
}

/// Prepared isolate-call helper returned by [`call`].
#[doc(hidden)]
pub struct IsolateCall<T, R> {
    destination: Address<T>,
    message: T,
    timeout: Duration,
    marker: std::marker::PhantomData<fn() -> R>,
}

impl<T> ObservedSend<T>
where
    T: Send + 'static,
{
    fn new(destination: Address<T>, message: T) -> Self {
        Self {
            destination,
            message,
        }
    }

    /// Turns this prepared observed send into one ordinary later message.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(SendOutcome) -> M + 'static,
        M: 'static,
    {
        tina::Effect::Call(RuntimeCall::observed_send(
            self.destination,
            self.message,
            translator,
        ))
    }
}

/// Returns a helper that attempts one send and later reports its outcome.
///
/// This is the overload-aware companion to ordinary fire-and-forget
/// [`tina::send`]. For cross-shard sends, [`SendOutcome::Accepted`] means the
/// source shard accepted the message into bounded transport toward the target
/// shard; destination-local mailbox failure is still recorded on the
/// destination trace.
pub fn send_observed<T>(destination: Address<T>, message: T) -> ObservedSend<T>
where
    T: Send + 'static,
{
    ObservedSend::new(destination, message)
}

impl<T, R> IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    fn new(destination: Address<T>, message: T, timeout: Duration) -> Self {
        Self {
            destination,
            message,
            timeout,
            marker: std::marker::PhantomData,
        }
    }

    /// Turns this prepared call into one ordinary later message.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(CallOutcome<R>) -> M + 'static,
        M: 'static,
    {
        tina::Effect::Call(RuntimeCall::isolate_call(
            self.destination,
            self.message,
            self.timeout,
            translator,
        ))
    }
}

/// Returns a helper that calls another isolate and requires a timeout.
pub fn call<T, R>(destination: Address<T>, message: T, timeout: Duration) -> IsolateCall<T, R>
where
    T: Send + 'static,
    R: 'static,
{
    IsolateCall::new(destination, message, timeout)
}

impl<T> TypedCall<T> {
    fn new(request: CallInput, decode: fn(CallOutput) -> Result<T, CallError>) -> Self {
        Self { request, decode }
    }

    /// Turns this prepared runtime-owned call into one ordinary later message.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(Result<T, CallError>) -> M + 'static,
        T: 'static,
    {
        let decode = self.decode;
        tina::Effect::Call(RuntimeCall::new(self.request, move |output| {
            translator(decode(output))
        }))
    }
}

/// Returns a typed sleep helper that later yields `Result<(), CallError>`.
pub fn sleep(after: Duration) -> TypedCall<()> {
    TypedCall::new(CallInput::Sleep { after }, CallOutput::into_timer_fired)
}

/// Returns a typed TCP bind helper that later yields one listener id and
/// bound address.
pub fn tcp_bind(addr: SocketAddr) -> TypedCall<(ListenerId, SocketAddr)> {
    TypedCall::new(CallInput::TcpBind { addr }, CallOutput::into_tcp_bound)
}

/// Returns a typed TCP accept helper that later yields one stream id and peer
/// address.
pub fn tcp_accept(listener: ListenerId) -> TypedCall<(StreamId, SocketAddr)> {
    TypedCall::new(
        CallInput::TcpAccept { listener },
        CallOutput::into_tcp_accepted,
    )
}

/// Returns a typed TCP read helper that later yields the bytes read.
pub fn tcp_read(stream: StreamId, max_len: usize) -> TypedCall<Vec<u8>> {
    TypedCall::new(
        CallInput::TcpRead { stream, max_len },
        CallOutput::into_tcp_read,
    )
}

/// Returns a typed TCP write helper that later yields the accepted byte count.
pub fn tcp_write(stream: StreamId, bytes: Vec<u8>) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::TcpWrite { stream, bytes },
        CallOutput::into_tcp_wrote,
    )
}

/// Returns a typed listener-close helper that later yields `Result<(), CallError>`.
pub fn tcp_close_listener(listener: ListenerId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TcpListenerClose { listener },
        CallOutput::into_tcp_listener_closed,
    )
}

/// Returns a typed stream-close helper that later yields `Result<(), CallError>`.
pub fn tcp_close_stream(stream: StreamId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TcpStreamClose { stream },
        CallOutput::into_tcp_stream_closed,
    )
}
